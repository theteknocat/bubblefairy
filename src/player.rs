//! Local video playback: GStreamer's `playbin` drawing through `gtk4paintablesink` into a `gdk::Paintable`
//! that the viewer's canvas paints. The file is a local copy (see `Cmd::Download`), so it can seek freely.

use gstreamer as gst;
use gstreamer::prelude::*;
use gtk::gdk;
use gtk::prelude::*;

/// What the player tells the viewer on its own.
pub enum Msg {
    /// played to the end
    Ended,
    Failed(String),
}

pub struct Player {
    pipeline: gst::Element,
    pub paintable: gdk::Paintable,
    _watch: gst::bus::BusWatchGuard,
}

impl Player {
    /// A paused player for the file at `path`; `on_msg` hears about the end and about errors.
    pub fn new(path: &std::path::Path, on_msg: impl Fn(Msg) + 'static) -> Result<Self, String> {
        let uri = gtk::gio::File::for_path(path).uri();
        let sink = gst::ElementFactory::make("gtk4paintablesink").build().map_err(|_| "the video output isn't available".to_string())?;
        let paintable = sink.property::<gdk::Paintable>("paintable");
        // with OpenGL available the frames stay on the graphics card
        let video_sink = gst::ElementFactory::make("glsinkbin").property("sink", &sink).build().unwrap_or(sink);
        let pipeline = gst::ElementFactory::make("playbin")
            .property("uri", uri.as_str())
            .property("video-sink", &video_sink)
            .build()
            .map_err(|_| "the player isn't available".to_string())?;
        let bus = pipeline.bus().ok_or("no message bus")?;
        let watch = bus
            .add_watch_local(move |_, msg| {
                match msg.view() {
                    gst::MessageView::Eos(_) => on_msg(Msg::Ended),
                    gst::MessageView::Error(e) => on_msg(Msg::Failed(e.error().to_string())),
                    _ => {}
                }
                glib_continue()
            })
            .map_err(|e| e.to_string())?;
        pipeline.set_state(gst::State::Paused).map_err(|_| "couldn't start the player".to_string())?;
        Ok(Self { pipeline, paintable, _watch: watch })
    }

    pub fn play(&self) {
        let _ = self.pipeline.set_state(gst::State::Playing);
    }

    pub fn pause(&self) {
        let _ = self.pipeline.set_state(gst::State::Paused);
    }

    pub fn is_playing(&self) -> bool {
        self.pipeline.current_state() == gst::State::Playing
    }

    /// (position, length) in seconds, once known
    pub fn times(&self) -> (f64, Option<f64>) {
        let pos = self.pipeline.query_position::<gst::ClockTime>().map_or(0.0, |t| t.seconds_f64());
        let len = self.pipeline.query_duration::<gst::ClockTime>().map(|t| t.seconds_f64());
        (pos, len)
    }

    pub fn seek(&self, secs: f64) {
        let t = gst::ClockTime::from_nseconds((secs.max(0.0) * 1e9) as u64);
        let _ = self.pipeline.seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT, t);
    }

    pub fn set_volume(&self, v: f64) {
        self.pipeline.set_property("volume", v.clamp(0.0, 1.0));
    }

    pub fn set_muted(&self, m: bool) {
        self.pipeline.set_property("mute", m);
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn glib_continue() -> gtk::glib::ControlFlow {
    gtk::glib::ControlFlow::Continue
}
