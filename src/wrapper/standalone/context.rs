use crossbeam::channel;
use std::sync::Arc;

use super::backend::Backend;
use super::wrapper::{GuiTask, Wrapper};
use crate::context::{GuiContext, PluginApi, ProcessContext, Transport};
use crate::midi::NoteEvent;
use crate::param::internals::ParamPtr;
use crate::plugin::Plugin;

/// A [`GuiContext`] implementation for the wrapper. This is passed to the plugin in
/// [`Editor::spawn()`][crate::prelude::Editor::spawn()] so it can interact with the rest of the plugin and
/// with the host for things like setting parameters.
pub(crate) struct WrapperGuiContext<P: Plugin, B: Backend> {
    pub(super) wrapper: Arc<Wrapper<P, B>>,

    /// This allows us to send tasks to the parent view that will be handled at the start of its
    /// next frame.
    pub(super) gui_task_sender: channel::Sender<GuiTask>,
}

/// A [`ProcessContext`] implementation for the standalone wrapper. This is a separate object so it
/// can hold on to lock guards for event queues. Otherwise reading these events would require
/// constant unnecessary atomic operations to lock the uncontested RwLocks.
pub(crate) struct WrapperProcessContext<'a, P: Plugin, B: Backend> {
    #[allow(dead_code)]
    pub(super) wrapper: &'a Wrapper<P, B>,
    // TODO: Events
    // pub(super) input_events_guard: AtomicRefMut<'a, VecDeque<NoteEvent>>,
    // pub(super) output_events_guard: AtomicRefMut<'a, VecDeque<NoteEvent>>,
    pub(super) transport: Transport,
}

impl<P: Plugin, B: Backend> GuiContext for WrapperGuiContext<P, B> {
    fn plugin_api(&self) -> PluginApi {
        PluginApi::Standalone
    }

    fn request_resize(&self) -> bool {
        let (unscaled_width, unscaled_height) = self.wrapper.editor.as_ref().unwrap().size();

        // This will cause the editor to be resized at the start of the next frame
        let dpi_scale = self.wrapper.dpi_scale();
        let push_successful = self
            .gui_task_sender
            .send(GuiTask::Resize(
                (unscaled_width as f32 * dpi_scale).round() as u32,
                (unscaled_height as f32 * dpi_scale).round() as u32,
            ))
            .is_ok();
        nih_debug_assert!(push_successful, "Could not queue window resize");

        true
    }

    unsafe fn raw_begin_set_parameter(&self, _param: ParamPtr) {
        // Since there's no autmoation being recorded here, gestures don't mean anything
    }

    unsafe fn raw_set_parameter_normalized(&self, param: ParamPtr, normalized: f32) {
        self.wrapper.set_parameter(param, normalized);
    }

    unsafe fn raw_end_set_parameter(&self, _param: ParamPtr) {}

    fn get_state(&self) -> crate::wrapper::state::PluginState {
        self.wrapper.get_state_object()
    }

    fn set_state(&self, state: crate::wrapper::state::PluginState) {
        self.wrapper.set_state_object(state)
    }
}

impl<P: Plugin, B: Backend> ProcessContext for WrapperProcessContext<'_, P, B> {
    fn plugin_api(&self) -> PluginApi {
        PluginApi::Standalone
    }

    fn transport(&self) -> &Transport {
        &self.transport
    }

    fn next_event(&mut self) -> Option<NoteEvent> {
        nih_debug_assert_failure!("TODO: WrapperProcessContext::next_event()");

        // self.input_events_guard.pop_front()
        None
    }

    fn send_event(&mut self, _event: NoteEvent) {
        nih_debug_assert_failure!("TODO: WrapperProcessContext::send_event()");

        // self.output_events_guard.push_back(event);
    }

    fn set_latency_samples(&self, _samples: u32) {
        nih_debug_assert_failure!("TODO: WrapperProcessContext::set_latency_samples()");
    }
}
