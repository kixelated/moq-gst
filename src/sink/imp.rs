use anyhow::Context as _;
use bytes::Bytes;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::Pad;

use moq_karp::{catalog, moq_transfork};
use moq_native::{quic, tls};
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

pub static RUNTIME: Lazy<tokio::runtime::Runtime> = Lazy::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(1)
        .build()
        .unwrap()
});

#[derive(Default)]
struct Settings {
    pub url: Option<String>,
    pub path: Vec<String>,
    pub tls_disable_verify: bool,
}

#[derive(Default)]
struct State {
    pub broadcast: Option<moq_karp::produce::Broadcast>,
    pub tracks: HashMap<String, moq_karp::produce::Track>,
    pub session: Option<moq_transfork::Session>,
}

#[derive(Default)]
pub struct MoqSink {
    settings: Mutex<Settings>,
    state: Arc<Mutex<State>>,
}

#[glib::object_subclass]
impl ObjectSubclass for MoqSink {
    const NAME: &'static str = "MoqSink";
    type Type = super::MoqSink;
    type ParentType = gst::Element;

    fn new() -> Self {
        Self::default()
    }
}

impl GstObjectImpl for MoqSink {}

impl ObjectImpl for MoqSink {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecString::builder("url")
                    .nick("URL")
                    .blurb("Connect to the subscriber at the given URL")
                    .build(),
                // TODO array of paths
                glib::ParamSpecString::builder("path")
                    .nick("Path")
                    .blurb("Publish the broadcast under the given path")
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
            "url" => settings.url = Some(value.get().unwrap()),
            "path" => settings.path = vec![value.get().unwrap()],
            "tls-disable-verify" => settings.tls_disable_verify = value.get().unwrap(),
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();

        match pspec.name() {
            "url" => settings.url.to_value(),
            "path" => settings.path.to_value(),
            "tls-disable-verify" => settings.tls_disable_verify.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl BinImpl for MoqSink {}
impl ElementImpl for MoqSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "MoQ Sink",
                "Sink",
                "Transmits media over the network via MoQ",
                "Luke Curley <kixelated@gmail.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            // Copied from the fmp4mux source
            let pad_template = gst::PadTemplate::new(
                "sink_%u",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &[
                    gst::Structure::builder("video/x-h264")
                        .field("stream-format", "avc")
                        .field("alignment", "au")
                        .build(),
                    /*
                    gst::Structure::builder("video/x-h265")
                        .field("stream-format", gst::List::new(["hvc1", "hev1"]))
                        .field("alignment", "au")
                        .field("parsed", true)
                        .build(),
                        */
                    /* TODO
                    gst::Structure::builder("video/x-vp8")
                        .field("width", gst::IntRange::new(1, u16::MAX as i32))
                        .field("height", gst::IntRange::new(1, u16::MAX as i32))
                        .build(),
                    gst::Structure::builder("video/x-vp9")
                        .field("profile", gst::List::new(["0", "1", "2", "3"]))
                        .field("chroma-format", gst::List::new(["4:2:0", "4:2:2", "4:4:4"]))
                        .field("bit-depth-luma", gst::List::new([8u32, 10u32, 12u32]))
                        .field("bit-depth-chroma", gst::List::new([8u32, 10u32, 12u32]))
                        .field("width", gst::IntRange::new(1, u16::MAX as i32))
                        .field("height", gst::IntRange::new(1, u16::MAX as i32))
                        .build(),
                    */
                    /*
                    gst::Structure::builder("video/x-av1")
                        .field("stream-format", "obu-stream")
                        .field("alignment", "tu")
                        .field("profile", gst::List::new(["main", "high", "professional"]))
                        .field(
                            "chroma-format",
                            gst::List::new(["4:0:0", "4:2:0", "4:2:2", "4:4:4"]),
                        )
                        .field("bit-depth-luma", gst::List::new([8u32, 10u32, 12u32]))
                        .field("bit-depth-chroma", gst::List::new([8u32, 10u32, 12u32]))
                        .field("width", gst::IntRange::new(1, u16::MAX as i32))
                        .field("height", gst::IntRange::new(1, u16::MAX as i32))
                        .build(),
                        */
                    /* TODO
                    gst::Structure::builder("audio/mpeg")
                        .field("mpegversion", 4i32)
                        .field("stream-format", "raw")
                        .field("channels", gst::IntRange::new(1, u16::MAX as i32))
                        .field("rate", gst::IntRange::new(1, i32::MAX))
                        .build(),
                    */
                    /*
                    gst::Structure::builder("audio/x-opus")
                        .field("channel-mapping-family", gst::IntRange::new(0i32, 255))
                        .field("channels", gst::IntRange::new(1i32, 8))
                        .field("rate", gst::IntRange::new(1, i32::MAX))
                        .build(),
                        */
                    /* TODO
                    gst::Structure::builder("audio/x-flac")
                        .field("framed", true)
                        .field("channels", gst::IntRange::<i32>::new(1, 8))
                        .field("rate", gst::IntRange::<i32>::new(1, 10 * u16::MAX as i32))
                        .build(),
                    */
                ]
                .into_iter()
                .collect::<gst::Caps>(),
            )
            .unwrap();

            vec![pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn request_new_pad(
        &self,
        templ: &gst::PadTemplate,
        name: Option<&str>,
        caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        let name = name.unwrap_or("sink_%u");
        let mut state = self.state.lock().unwrap();

        // Check if caps is None and handle it gracefully
        if caps.is_none() {
            println!("request_new_pad: Caps is None, creating pad with default or template caps.");
            let pad = gst::PadBuilder::from_template(templ).name(name).build();
            self.obj().add_pad(&pad).unwrap();

            // Configure additional pad settings if needed before returning
            return Some(pad);
        }

        println!("request_new_pad: {:?} {} {:?}", templ, name, caps);
        let intersected_caps = templ
            .caps()
            .intersect(caps.unwrap_or(&gst::Caps::new_empty()));
        println!("intersected_caps: {:?}", intersected_caps);

        let s = caps?.structure(0).unwrap();
        let track = match s.name().as_str() {
            "video/x-h264" => {
                let description = s
                    .get::<&gst::BufferRef>("codec_data")
                    .expect("no codec_data")
                    .map_readable()
                    .expect("failed to map codec_data");

                let constraints = s.get::<u8>("constraints").unwrap_or(0);
                let profile = match s.get::<&str>("profile").expect("no profile") {
                    "baseline" => 66,
                    "main" => 77,
                    "high" => 100,
                    _ => panic!("unknown profile"),
                };

                // wtf is this
                let level = s.get::<&str>("level").expect("no level");
                let level = match level {
                    "1" => 10,
                    "1b" => 9,
                    "1.1" => 11,
                    "1.2" => 12,
                    "1.3" => 13,
                    "2" => 20,
                    "2.1" => 21,
                    "2.2" => 22,
                    "3" => 30,
                    "3.1" => 31,
                    "3.2" => 32,
                    "4" => 40,
                    "4.1" => 41,
                    "4.2" => 42,
                    "5" => 50,
                    "5.1" => 51,
                    "5.2" => 52,
                    _ => panic!("unknown level"),
                };

                let video = catalog::Video {
                    track: catalog::Track {
                        name: name.to_string(),
                        priority: 1,
                    },
                    codec: catalog::H264 {
                        constraints,
                        profile,
                        level,
                    }
                    .into(),
                    resolution: catalog::Dimensions {
                        width: s.get::<u32>("width").unwrap() as _,
                        height: s.get::<u32>("height").unwrap() as _,
                    },
                    bitrate: s.get::<u32>("bitrate").ok(),
                    description: Bytes::copy_from_slice(&description),
                };

                state
                    .broadcast
                    .as_mut()
                    .unwrap()
                    .create_video(video)
                    .unwrap()
            }
            "audio/x-opus" => {
                let audio = catalog::Audio {
                    track: catalog::Track {
                        name: name.to_string(),
                        priority: 2,
                    },
                    codec: catalog::AudioCodec::Opus,
                    bitrate: s.get::<u32>("bitrate").ok(),
                    sample_rate: s.get::<u32>("rate").unwrap() as _,
                    channel_count: s.get::<u32>("channels").unwrap() as _,
                };

                state
                    .broadcast
                    .as_mut()
                    .unwrap()
                    .create_audio(audio)
                    .unwrap()
            }
            _ => {
                panic!("unsupported media type: {}", s.name());
            }
        };

        state.tracks.insert(name.to_string(), track);

        let pad = Pad::builder_from_template(templ)
            .name(name)
            .event_function(|pad, parent, event| {
                parent
                    .unwrap()
                    .downcast_ref::<super::MoqSink>()
                    .expect("Parent object is not set")
                    .imp()
                    .handle_event(pad, event)
            })
            .chain_function(|pad, parent, buffer| {
                parent
                    .unwrap()
                    .downcast_ref::<super::MoqSink>()
                    .unwrap()
                    .imp()
                    .process_buffer(pad, buffer)
            })
            .build();

        // Add the pad to the element
        self.obj().add_pad(&pad).unwrap();

        Some(pad)
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        match transition {
            gst::StateChange::NullToReady => {
                let _guard = RUNTIME.enter();
                self.setup().expect("failed to setup");
            }
            _ => {}
        }
        self.parent_change_state(transition)
    }
}

impl MoqSink {
    fn process_buffer(
        &self,
        pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let _guard = RUNTIME.enter();

        let mut state = self.state.lock().unwrap();

        let payload = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
        let payload = Bytes::copy_from_slice(payload.as_slice());

        let timestamp = buffer.pts().unwrap_or(buffer.dts().unwrap());
        let timestamp = moq_karp::media::Timestamp::from_micros(timestamp.useconds());

        let keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);

        let frame = moq_karp::media::Frame {
            timestamp,
            keyframe,
            payload,
        };

        let track = state
            .tracks
            .get_mut(pad.name().as_str())
            .expect("unknown pad");

        track.write(frame);

        Ok(gst::FlowSuccess::Ok)
    }

    fn handle_event(&self, _pad: &gst::Pad, event: gst::Event) -> bool {
        match event.view() {
            gst::EventView::Eos(_) => {
                // Handle EOS if necessary
                true
            }
            _ => true,
        }
    }
}

/*
impl BaseSinkImpl for MoqSink {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        Ok(())
    }

    fn render(&self, buffer: &gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
        let _guard = RUNTIME.enter();
        let data = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;

        let mut state = self.state.lock().unwrap();
        let mut media = state.media.take().expect("not initialized");

        // TODO avoid full media parsing? gst should be able to provide the necessary info
        media.parse(data.as_slice()).expect("failed to parse");

        if !state.published {
            if let Some(session) = state.session.as_mut() {
                media.publish(session).expect("failed to publish");
                state.published = true;
            }
        }

        state.media = Some(media);

        Ok(gst::FlowSuccess::Ok)
    }
}
*/

impl MoqSink {
    fn setup(&self) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap();
        let settings = self.settings.lock().unwrap();

        let url = settings.url.clone().context("missing url")?;
        let url = url.parse().context("invalid URL")?;
        let path = moq_transfork::Path::new(settings.path.clone());

        let broadcast = moq_karp::produce::Resumable::new(path).broadcast();
        state.broadcast = Some(broadcast);

        // TODO support TLS certs and other options
        let config = quic::Args {
            bind: "[::]:0".parse().unwrap(),
            tls: tls::Args {
                disable_verify: settings.tls_disable_verify,
                ..Default::default()
            },
        }
        .load()?;

        let client = quic::Endpoint::new(config)?.client;
        let state = self.state.clone();

        // We have to perform the connect in a background task because we can't block the main thread
        tokio::spawn(async move {
            let session = client.connect(&url).await.expect("failed to connect");
            let mut session = moq_transfork::Session::connect(session)
                .await
                .expect("failed to connect");

            let mut state = state.lock().unwrap();
            state
                .broadcast
                .as_mut()
                .unwrap()
                .publish(&mut session)
                .expect("failed to publish");

            state.session = Some(session);

            // TODO figure out how to close gstreamer gracefully on session close
        });

        Ok(())
    }
}
