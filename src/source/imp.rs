use anyhow::Context as _;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use moq_karp::moq_transfork;

use moq_native::{quic, tls};
use once_cell::sync::Lazy;
use std::sync::LazyLock;
use std::sync::Mutex;

static CAT: Lazy<gst::DebugCategory> =
	Lazy::new(|| gst::DebugCategory::new("moqsrc", gst::DebugColorFlags::empty(), Some("MoQ Source Element")));

pub static RUNTIME: Lazy<tokio::runtime::Runtime> = Lazy::new(|| {
	tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.worker_threads(1)
		.build()
		.unwrap()
});

#[derive(Default, Clone)]
struct Settings {
	pub url: String,
	pub tls_disable_verify: bool,
}

#[derive(Default)]
pub(crate) struct MoqSrcPad {}

#[glib::object_subclass]
impl ObjectSubclass for MoqSrcPad {
	const NAME: &'static str = "MoqSrcPad";
	type Type = super::MoqSrcPad;
	type ParentType = gst::GhostPad;
}

impl ObjectImpl for MoqSrcPad {}
impl GstObjectImpl for MoqSrcPad {}
impl PadImpl for MoqSrcPad {}
impl ProxyPadImpl for MoqSrcPad {}
impl GhostPadImpl for MoqSrcPad {}

#[derive(Default)]
pub struct MoqSrc {
	settings: Mutex<Settings>,
}

#[glib::object_subclass]
impl ObjectSubclass for MoqSrc {
	const NAME: &'static str = "MoqSrc";
	type Type = super::MoqSrc;
	type ParentType = gst::Bin;

	fn new() -> Self {
		Self::default()
	}
}

impl GstObjectImpl for MoqSrc {}
impl BinImpl for MoqSrc {}

impl ObjectImpl for MoqSrc {
	fn properties() -> &'static [glib::ParamSpec] {
		static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
			vec![
				glib::ParamSpecString::builder("url")
					.nick("Source URL")
					.blurb("Connect to the given URL")
					.build(),
				glib::ParamSpecBoolean::builder("tls-disable-verify")
					.nick("TLS disable verify")
					.blurb("Disable TLS verification")
					.default_value(false)
					.build(),
			]
		});
		PROPERTIES.as_ref()
	}

	fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
		let mut settings = self.settings.lock().unwrap();

		match pspec.name() {
			"url" => settings.url = value.get().unwrap(),
			"tls-disable-verify" => settings.tls_disable_verify = value.get().unwrap(),
			_ => unimplemented!(),
		}
	}

	fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
		let settings = self.settings.lock().unwrap();

		match pspec.name() {
			"url" => settings.url.to_value(),
			"tls-disable-verify" => settings.tls_disable_verify.to_value(),
			_ => unimplemented!(),
		}
	}
}

impl ElementImpl for MoqSrc {
	fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
		static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
			gst::subclass::ElementMetadata::new(
				"MoQ Src",
				"Source/Network/MoQ",
				"Receives media over the network via MoQ",
				"Luke Curley <kixelated@gmail.com>",
			)
		});

		Some(&*ELEMENT_METADATA)
	}

	fn pad_templates() -> &'static [gst::PadTemplate] {
		static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
			let pad = gst::PadTemplate::new(
				"src_%u",
				gst::PadDirection::Src,
				gst::PadPresence::Sometimes,
				&gst::Caps::new_any(),
			)
			.unwrap();

			vec![pad]
		});

		PAD_TEMPLATES.as_ref()
	}

	fn change_state(&self, transition: gst::StateChange) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
		match transition {
			gst::StateChange::ReadyToPaused => {
				if let Err(e) = RUNTIME.block_on(self.setup()) {
					gst::error!(CAT, obj = self.obj(), "Failed to setup: {:?}", e);
					return Err(gst::StateChangeError);
				}
			}

			gst::StateChange::PausedToReady => {
				// Cleanup publisher
				self.cleanup();
			}

			_ => (),
		}

		// Chain up
		self.parent_change_state(transition)
	}
}

impl MoqSrc {
	async fn setup(&self) -> anyhow::Result<()> {
		let settings = self.settings.lock().unwrap();
		let url = url::Url::parse(&settings.url)?;
		let path = url.path().to_string();

		// TODO support TLS certs and other options
		let config = quic::Args {
			bind: "[::]:0".parse().unwrap(),
			tls: tls::Args {
				disable_verify: settings.tls_disable_verify,
				..Default::default()
			},
		}
		.load()?;
		drop(settings);

		let client = quic::Endpoint::new(config)?.client;

		let session = client.connect(url).await?;
		let session = moq_transfork::Session::connect(session).await?;
		let mut broadcast = moq_karp::BroadcastConsumer::new(session, path);

		// TODO handle catalog updates
		let catalog = broadcast.next_catalog().await?.context("no catalog found")?.clone();

		gst::info!(CAT, "catalog: {:?}", catalog);

		for video in catalog.video {
			let mut track = broadcast.track(&video.track)?;

			let caps = match video.codec {
				moq_karp::VideoCodec::H264(_) => {
					let builder = gst::Caps::builder("video/x-h264")
						.field("width", video.resolution.width)
						.field("height", video.resolution.height)
						.field("alignment", "au");

					if let Some(description) = video.description {
						builder
							.field("stream-format", "avc")
							.field("codec_data", gst::Buffer::from_slice(description.clone()))
							.build()
					} else {
						builder.field("stream-format", "annexb").build()
					}
				}
				_ => unimplemented!(),
			};

			let appsrc = gst_app::AppSrc::builder()
				.name(&video.track.name)
				.caps(&caps)
				.format(gst::Format::Time)
				.is_live(true)
				.stream_type(gst_app::AppStreamType::Stream)
				.do_timestamp(true)
				.build();

			let appsrc_pad = appsrc.static_pad("src").unwrap();

			let templ = self.obj().pad_template("src_%u").unwrap();
			let srcpad = gst::GhostPad::builder_from_template(&templ)
				.name(format!("src_{}", 0))
				.build();

			srcpad.set_target(Some(&appsrc_pad))?;
			srcpad.set_active(true)?;

			self.obj().add_pad(&srcpad)?;
			self.obj().add(&appsrc)?;

			tokio::spawn(async move {
				// TODO don't panic on error
				while let Some(frame) = track.read().await.expect("failed to read frame") {
					// TODO
					let mut buffer = gst::Buffer::from_slice(frame.payload);
					let buffer_mut = buffer.get_mut().unwrap();

					let pts = gst::ClockTime::from_nseconds(frame.timestamp.as_nanos() as _);
					buffer_mut.set_pts(Some(pts));

					let mut flags = buffer_mut.flags();
					if frame.keyframe {
						flags.insert(gst::BufferFlags::MARKER);
					} else {
						flags.insert(gst::BufferFlags::DELTA_UNIT);
					}
					buffer_mut.set_flags(flags);

					// Ensure appsrc has caps set
					if appsrc.caps().is_none() {
						gst::error!(CAT, "AppSrc missing caps!");
						break;
					}

					let sample = gst::Sample::builder()
						.buffer(&buffer)
						.caps(&appsrc.caps().unwrap())
						.build();
					if let Err(err) = appsrc.push_sample(&sample) {
						gst::warning!(CAT, "Failed to push sample: {:?}", err);
					}
				}

				appsrc.end_of_stream().unwrap();
			});
		}

		for audio in catalog.audio {}

		// We downloaded the catalog and created all the pads.
		self.obj().no_more_pads();

		Ok(())
	}

	fn cleanup(&self) {
		// TODO kill spawned tasks
	}
}
