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
	type Interfaces = (gst::ChildProxy,);

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
			// Caps are restricted by the cmafmux element negotiation inside our bin element
			let sink_pad_template = gst::PadTemplate::with_gtype(
				"src_%u",
				gst::PadDirection::Src,
				gst::PadPresence::Request,
				&gst::Caps::new_any(),
				super::MoqSrcPad::static_type(),
			)
			.unwrap();

			vec![sink_pad_template]
		});

		PAD_TEMPLATES.as_ref()
	}

	fn change_state(&self, transition: gst::StateChange) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
		match transition {
			gst::StateChange::ReadyToPaused => {
				if let Err(e) = self.setup() {
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

impl ChildProxyImpl for MoqSrc {
	fn children_count(&self) -> u32 {
		let object = self.obj();
		object.num_pads() as u32
	}

	fn child_by_name(&self, name: &str) -> Option<glib::Object> {
		let object = self.obj();
		object.pads().into_iter().find(|p| p.name() == name).map(|p| p.upcast())
	}

	fn child_by_index(&self, index: u32) -> Option<glib::Object> {
		let object = self.obj();
		object.pads().into_iter().nth(index as usize).map(|p| p.upcast())
	}
}

impl MoqSrc {
	fn setup(&self) -> anyhow::Result<()> {
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

		let (broadcast, catalog) = RUNTIME.block_on(async {
			let session = client.connect(url).await?;
			let session = moq_transfork::Session::connect(session).await?;
			let mut broadcast = moq_karp::BroadcastConsumer::new(session, path);

			// TODO handle catalog updates
			let catalog = broadcast.next_catalog().await?.context("no catalog found")?.clone();

			Ok::<_, anyhow::Error>((broadcast, catalog))
		})?;

		for video in catalog.video {
			let mut track = broadcast.track(&video.track)?;

			let caps = gst::Caps::builder("video")
				.field("codec", video.codec.to_string())
				.field("width", video.resolution.width)
				.field("height", video.resolution.height)
				.build();

			let appsrc = gst::ElementFactory::make("appsrc")
				.name(&video.track.name)
				.build()
				.unwrap();

			appsrc.set_property("format", gst::Format::Time);
			appsrc.set_property("caps", &caps);
			appsrc.set_property("blocksize", 4096u32);

			self.obj().add(&appsrc)?;

			let srcpad = appsrc.static_pad("src").unwrap();
			let ghostpad = gst::GhostPad::with_target(&srcpad).unwrap();
			ghostpad.set_active(true)?;
			self.obj().add_pad(&ghostpad)?;

			tokio::spawn(async move {
				// TODO don't panic on error
				while let Some(frame) = track.read().await.expect("failed to read frame") {
					// TODO
					let buffer = gst::Buffer::from_slice(frame.payload);
					/*
					buffer.set_pts(Some(gst::ClockTime::from_nseconds(frame.timestamp.as_nanos() as _)));

					let flags = buffer.flags();
					if frame.keyframe {
						buffer.set_flags(flags | gst::BufferFlags::MARKER);
					} else {
						buffer.set_flags(flags & !gst::BufferFlags::DELTA_UNIT);
					}
					*/

					appsrc.emit_by_name::<()>("push-buffer", &[&buffer]);
				}
			});
		}

		for audio in catalog.audio {}

		Ok(())
	}

	fn cleanup(&self) {
		// TODO kill spawned tasks
	}
}
