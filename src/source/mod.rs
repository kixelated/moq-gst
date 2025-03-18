use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
	pub struct MoqSrc(ObjectSubclass<imp::MoqSrc>) @extends gst::Bin, gst::Element, gst::Object, @implements gst::ChildProxy;
}

glib::wrapper! {
	pub(crate) struct MoqSrcPad(ObjectSubclass<imp::MoqSrcPad>) @extends gst::GhostPad, gst::ProxyPad, gst::Pad, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
	gst::Element::register(Some(plugin), "moqsrc", gst::Rank::NONE, MoqSrc::static_type())
}
