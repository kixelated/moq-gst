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
			let video = gst::PadTemplate::new(
				"video_%u",
				gst::PadDirection::Src,
				gst::PadPresence::Sometimes,
				&gst::Caps::new_any(),
			)
			.unwrap();

			let audio = gst::PadTemplate::new(
				"audio_%u",
				gst::PadDirection::Src,
				gst::PadPresence::Sometimes,
				&gst::Caps::new_any(),
			)
			.unwrap();

			vec![video, audio]
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
		let (quic, url, path) = {
			let settings = self.settings.lock().unwrap();
			let url = url::Url::parse(&settings.url)?;
			let path = url.path().strip_prefix("/").unwrap().to_string();

			// TODO support TLS certs and other options
			let quic = quic::Args {
				bind: "[::]:0".parse().unwrap(),
				tls: tls::Args {
					disable_verify: settings.tls_disable_verify,
					..Default::default()
				},
			};

			(quic, url, path)
		};

		let quic = quic.load()?;
		let client = quic::Endpoint::new(quic)?.client;

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
						//.field("width", video.resolution.width)
						//.field("height", video.resolution.height)
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

			gst::info!(CAT, "caps: {:?}", caps);

			let templ = self.obj().element_class().pad_template("video_%u").unwrap();

			let srcpad = gst::Pad::builder_from_template(&templ).name(&video.track.name).build();
			srcpad.set_active(true).unwrap();

			let stream_start = gst::event::StreamStart::builder(&video.track.name)
				.group_id(gst::GroupId::next())
				.build();
			srcpad.push_event(stream_start);

			let caps_evt = gst::event::Caps::new(&caps);
			srcpad.push_event(caps_evt);

			let segment = gst::event::Segment::new(&gst::FormattedSegment::<gst::ClockTime>::new());
			srcpad.push_event(segment);

			self.obj().add_pad(&srcpad).expect("Failed to add pad");

			let mut reference = None;

			// Push to the srcpad in a background task.
			tokio::spawn(async move {
				// TODO don't panic on error
				while let Some(frame) = track.read().await.expect("failed to read frame") {
					let mut buffer = gst::Buffer::from_slice(frame.payload);
					let buffer_mut = buffer.get_mut().unwrap();

					// Make the timestamps relative to the first frame
					let timestamp = if let Some(reference) = reference {
						frame.timestamp - reference
					} else {
						reference = Some(frame.timestamp);
						frame.timestamp
					};

					let pts = gst::ClockTime::from_nseconds(timestamp.as_nanos() as _);
					buffer_mut.set_pts(Some(pts));

					let mut flags = buffer_mut.flags();
					match frame.keyframe {
						true => flags.remove(gst::BufferFlags::DELTA_UNIT),
						false => flags.insert(gst::BufferFlags::DELTA_UNIT),
					};

					buffer_mut.set_flags(flags);

					gst::info!(CAT, "pushing sample: {:?}", buffer);

					if let Err(err) = srcpad.push(buffer) {
						gst::warning!(CAT, "Failed to push sample: {:?}", err);
					}
				}
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
