//! A simple generic UI widget that renders all parameters in a [`Params`] object as a scrollable
//! list of sliders and labels.

use std::pin::Pin;

use egui::{TextStyle, Ui, Vec2};
use nih_plug::context::ParamSetter;
use nih_plug::param::internals::ParamPtr;
use nih_plug::prelude::{Param, Params};

use super::ParamSlider;

/// A widget that can be used to create a generic UI with. This is used in conjuction with empty
/// structs to emulate existential types.
pub trait ParamWidget {
    fn add_widget<P: Param>(&self, ui: &mut Ui, param: &P, setter: &ParamSetter);

    unsafe fn add_widget_raw(&self, ui: &mut Ui, param: &ParamPtr, setter: &ParamSetter) {
        match param {
            ParamPtr::FloatParam(p) => self.add_widget(ui, &**p, setter),
            ParamPtr::IntParam(p) => self.add_widget(ui, &**p, setter),
            ParamPtr::BoolParam(p) => self.add_widget(ui, &**p, setter),
            ParamPtr::EnumParam(p) => self.add_widget(ui, &**p, setter),
        }
    }
}

/// Create a generic UI using [`ParamSlider`]s.
pub struct GenericSlider;

/// Create a scrollable generic UI using the specified widget. Takes up all the remaining vertical
/// space.
pub fn create(
    ui: &mut Ui,
    params: Pin<&dyn Params>,
    setter: &ParamSetter,
    widget: impl ParamWidget,
) {
    let param_map = params.param_map();
    let param_ids = params.param_ids();

    let padding = Vec2::splat(ui.text_style_height(&TextStyle::Body) * 0.2);
    egui::containers::ScrollArea::vertical()
        // Take up all remaining space, use a wrapper container to adjust how much space that is
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (widget_idx, id) in param_ids.into_iter().enumerate() {
                let param = param_map[id];

                // This list looks weird without a little padding
                if widget_idx > 0 {
                    ui.allocate_space(padding);
                }

                ui.label(unsafe { param.name() });
                unsafe { widget.add_widget_raw(ui, &param, setter) };
            }
        });
}

impl ParamWidget for GenericSlider {
    fn add_widget<P: Param>(&self, ui: &mut Ui, param: &P, setter: &ParamSetter) {
        ui.add(ParamSlider::for_param(param, setter));
    }
}