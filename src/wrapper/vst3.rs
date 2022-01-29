// nih-plug: plugins, but rewritten in Rust
// Copyright (C) 2022 Robbert van der Helm
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

// The VST3 macro generates an `allocate()` function for initializing the struct, so Clippy will
// complain as soon as a struct has more than 8 fields
#![allow(clippy::too_many_arguments)]

use lazy_static::lazy_static;
use std::cell::{Cell, RefCell};
use std::cmp;
use std::collections::HashMap;
use std::ffi::c_void;
use std::marker::PhantomData;
use std::mem;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use vst3_sys::base::{kInvalidArgument, kNoInterface, kResultFalse, kResultOk, tresult, TBool};
use vst3_sys::base::{IPluginBase, IPluginFactory, IPluginFactory2, IPluginFactory3};
use vst3_sys::vst::TChar;
use vst3_sys::vst::{
    IAudioProcessor, IComponent, IEditController, IParamValueQueue, IParameterChanges,
};
use vst3_sys::VST3;
use widestring::U16CStr;

use crate::params::ParamPtr;
use crate::plugin::{BufferConfig, BusConfig, Plugin, ProcessStatus, Vst3Plugin};
use crate::wrapper::util::{hash_param_id, strlcpy, u16strlcpy};

// Alias needed for the VST3 attribute macro
use vst3_sys as vst3_com;

/// Re-export for the wrapper.
pub use vst3_sys::sys::GUID;

/// The VST3 SDK version this is roughtly based on.
const VST3_SDK_VERSION: &str = "VST 3.6.14";
/// Right now the wrapper adds its own bypass parameter.
///
/// TODO: Actually use this parameter.
const BYPASS_PARAM_ID: &str = "bypass";
lazy_static! {
    static ref BYPASS_PARAM_HASH: u32 = hash_param_id(BYPASS_PARAM_ID);
}

/// Early exit out of a VST3 function when one of the passed pointers is null
macro_rules! check_null_ptr {
    ($ptr:expr $(, $ptrs:expr)* $(, )?) => {
        check_null_ptr_msg!("Null pointer passed to function", $ptr $(, $ptrs)*)
    };
}

/// The same as [check_null_ptr], but with a custom message.
macro_rules! check_null_ptr_msg {
    ($msg:expr, $ptr:expr $(, $ptrs:expr)* $(, )?) => {
        if $ptr.is_null() $(|| $ptrs.is_null())* {
            nih_debug_assert_failure!($msg);
            return kInvalidArgument;
        }
    };
}

#[VST3(implements(IComponent, IEditController, IAudioProcessor))]
pub struct Wrapper<'a, P: Plugin> {
    /// The wrapped plugin instance.
    plugin: RefCell<P>,
    /// Whether the plugin is currently bypassed. This is not yet integrated with the `Plugin`
    /// trait.
    bypass_state: Cell<bool>,
    /// The last process status returned by the plugin. This is used for tail handling.
    last_process_status: Cell<ProcessStatus>,
    is_processing: AtomicBool,

    /// Contains slices for the plugin's outputs. You can't directly create a nested slice form
    /// apointer to pointers, so this needs to be preallocated in the setup call and kept around
    /// between process calls.
    output_slices: RefCell<Vec<&'a mut [f32]>>,

    /// A mapping from parameter ID hashes (obtained from the string parameter IDs) to pointers to
    /// parameters belonging to the plugin. As long as `plugin` does not get recreated, these
    /// addresses will remain stable, as they are obtained from a pinned object.
    param_by_hash: HashMap<u32, ParamPtr>,
    /// The keys from `param_map` in a stable order.
    param_hashes: Vec<u32>,
    /// The default normalized parameter value for every parameter in `param_ids`. We need to store
    /// this in case the host requeries the parmaeter later.
    param_defaults_normalized: Vec<f32>,
    /// Mappings from parameter hashes back to string parameter indentifiers. Useful for debug
    /// logging and when handling plugin state.
    param_id_hashes: HashMap<u32, &'static str>,

    /// The current bus configuration, modified through `IAudioProcessor::setBusArrangements()`.
    current_bus_config: RefCell<BusConfig>,
}

impl<P: Plugin> Wrapper<'_, P> {
    pub fn new() -> Box<Self> {
        let mut wrapper = Self::allocate(
            RefCell::new(P::default()),       // plugin
            Cell::new(false),                 // bypass_state
            Cell::new(ProcessStatus::Normal), // last_process_status
            AtomicBool::new(false),           // is_processing
            RefCell::new(Vec::new()),         // output_slices
            HashMap::new(),                   // param_by_hash
            Vec::new(),                       // param_hashes
            Vec::new(),                       // param_defaults_normalized
            HashMap::new(),                   // param_id_hashes
            // Some hosts, like the current version of Bitwig and Ardour at the time of writing,
            // will try using the plugin's default not yet initialized bus arrangement. Because of
            // that, we'll always initialize this configuration even before the host requests a
            // channel layout.
            RefCell::new(BusConfig {
                num_input_channels: P::DEFAULT_NUM_INPUTS,
                num_output_channels: P::DEFAULT_NUM_OUTPUTS,
            }),
        );

        // This is a mapping from the parameter IDs specified by the plugin to pointers to thsoe
        // parameters. Since the object returned by `params()` is pinned, these pointers are safe to
        // dereference as long as `wrapper.plugin` is alive
        let param_map = wrapper.plugin.borrow().params().param_map();
        nih_debug_assert!(
            !param_map.contains_key(BYPASS_PARAM_ID),
            "The wrapper alread yadds its own bypass parameter"
        );

        wrapper.param_by_hash = param_map
            .iter()
            .map(|(id, p)| (hash_param_id(id), *p))
            .collect();
        wrapper.param_hashes = wrapper.param_by_hash.keys().copied().collect();
        wrapper.param_defaults_normalized = wrapper
            .param_hashes
            .iter()
            .map(|hash| unsafe { wrapper.param_by_hash[hash].normalized_value() })
            .collect();
        wrapper.param_id_hashes = param_map
            .into_keys()
            .map(|id| (hash_param_id(id), id))
            .collect();

        wrapper
    }

    unsafe fn set_normalized_value_by_hash(&self, hash: u32, normalized_value: f64) -> tresult {
        if hash == *BYPASS_PARAM_HASH {
            self.bypass_state.set(normalized_value >= 0.5);

            kResultOk
        } else if let Some(param_ptr) = self.param_by_hash.get(&hash) {
            param_ptr.set_normalized_value(normalized_value as f32);

            kResultOk
        } else {
            kInvalidArgument
        }
    }
}

impl<P: Plugin> IPluginBase for Wrapper<'_, P> {
    unsafe fn initialize(&self, _context: *mut c_void) -> tresult {
        // We currently don't need or allow any initialization logic
        kResultOk
    }

    unsafe fn terminate(&self) -> tresult {
        kResultOk
    }
}

impl<P: Plugin> IComponent for Wrapper<'_, P> {
    unsafe fn get_controller_class_id(&self, _tuid: *mut vst3_sys::IID) -> tresult {
        // We won't separate the edit controller to keep the implemetnation a bit smaller
        kNoInterface
    }

    unsafe fn set_io_mode(&self, _mode: vst3_sys::vst::IoMode) -> tresult {
        // This would need to integrate with the GUI, which we currently don't have
        kResultOk
    }

    unsafe fn get_bus_count(
        &self,
        type_: vst3_sys::vst::MediaType,
        _dir: vst3_sys::vst::BusDirection,
    ) -> i32 {
        // All plugins currently only have a single input and a single output bus
        match type_ {
            x if x == vst3_sys::vst::MediaTypes::kAudio as i32 => 1,
            _ => 0,
        }
    }

    unsafe fn get_bus_info(
        &self,
        type_: vst3_sys::vst::MediaType,
        dir: vst3_sys::vst::BusDirection,
        index: i32,
        info: *mut vst3_sys::vst::BusInfo,
    ) -> tresult {
        check_null_ptr!(info);

        match type_ {
            t if t == vst3_sys::vst::MediaTypes::kAudio as i32 => {
                *info = mem::zeroed();

                let info = &mut *info;
                info.media_type = vst3_sys::vst::MediaTypes::kAudio as i32;
                info.bus_type = vst3_sys::vst::BusTypes::kMain as i32;
                info.flags = vst3_sys::vst::BusFlags::kDefaultActive as u32;
                match (dir, index) {
                    (d, 0) if d == vst3_sys::vst::BusDirections::kInput as i32 => {
                        info.direction = vst3_sys::vst::BusDirections::kInput as i32;
                        info.channel_count =
                            self.current_bus_config.borrow().num_input_channels as i32;
                        u16strlcpy(&mut info.name, "Input");

                        kResultOk
                    }
                    (d, 0) if d == vst3_sys::vst::BusDirections::kOutput as i32 => {
                        info.direction = vst3_sys::vst::BusDirections::kOutput as i32;
                        info.channel_count =
                            self.current_bus_config.borrow().num_output_channels as i32;
                        u16strlcpy(&mut info.name, "Output");

                        kResultOk
                    }
                    _ => kInvalidArgument,
                }
            }
            _ => kInvalidArgument,
        }
    }

    unsafe fn get_routing_info(
        &self,
        in_info: *mut vst3_sys::vst::RoutingInfo,
        out_info: *mut vst3_sys::vst::RoutingInfo,
    ) -> tresult {
        check_null_ptr!(in_info, out_info);

        *out_info = mem::zeroed();

        let in_info = &*in_info;
        let out_info = &mut *out_info;
        match (in_info.media_type, in_info.bus_index) {
            (t, 0) if t == vst3_sys::vst::MediaTypes::kAudio as i32 => {
                out_info.media_type = vst3_sys::vst::MediaTypes::kAudio as i32;
                out_info.bus_index = in_info.bus_index;
                out_info.channel = in_info.channel;

                kResultOk
            }
            _ => kInvalidArgument,
        }
    }

    unsafe fn activate_bus(
        &self,
        type_: vst3_sys::vst::MediaType,
        _dir: vst3_sys::vst::BusDirection,
        index: i32,
        _state: vst3_sys::base::TBool,
    ) -> tresult {
        // We don't need any special handling here
        match (type_, index) {
            (t, 0) if t == vst3_sys::vst::MediaTypes::kAudio as i32 => kResultOk,
            _ => kInvalidArgument,
        }
    }

    unsafe fn set_active(&self, _state: TBool) -> tresult {
        // We don't need any special handling here
        kResultOk
    }

    unsafe fn set_state(&self, _state: *mut c_void) -> tresult {
        // TODO: Implemnt state saving and restoring
        kResultFalse
    }

    unsafe fn get_state(&self, _state: *mut c_void) -> tresult {
        // TODO: Implemnt state saving and restoring
        kResultFalse
    }
}

impl<P: Plugin> IEditController for Wrapper<'_, P> {
    unsafe fn set_component_state(&self, _state: *mut c_void) -> tresult {
        // We have a single file component, so we don't need to do anything here
        kResultOk
    }

    unsafe fn set_state(&self, state: *mut c_void) -> tresult {
        // We have a single file component, so there's only one `set_state()` function. Unlike C++,
        // Rust allows you to have multiple methods with the same name when they're provided by
        // different treats, but because of the Rust implementation the host may call either of
        // these functions depending on how they're implemented
        IComponent::set_state(self, state)
    }

    unsafe fn get_state(&self, state: *mut c_void) -> tresult {
        // Same for this function
        IComponent::get_state(self, state)
    }

    unsafe fn get_parameter_count(&self) -> i32 {
        // NOTE: We add a bypass parameter ourselves on index `self.param_ids.len()`, so these
        //       indices are all off by one
        self.param_hashes.len() as i32 + 1
    }

    unsafe fn get_parameter_info(
        &self,
        param_index: i32,
        info: *mut vst3_sys::vst::ParameterInfo,
    ) -> tresult {
        check_null_ptr!(info);

        // Parameter index `self.param_ids.len()` is our own bypass parameter
        if param_index < 0 || param_index > self.param_hashes.len() as i32 {
            return kInvalidArgument;
        }

        *info = std::mem::zeroed();

        let info = &mut *info;
        if param_index == self.param_hashes.len() as i32 {
            info.id = *BYPASS_PARAM_HASH;
            u16strlcpy(&mut info.title, "Bypass");
            u16strlcpy(&mut info.short_title, "Bypass");
            u16strlcpy(&mut info.units, "");
            info.step_count = 0;
            info.default_normalized_value = 0.0;
            info.unit_id = vst3_sys::vst::kRootUnitId;
            info.flags = vst3_sys::vst::ParameterFlags::kCanAutomate as i32
                | vst3_sys::vst::ParameterFlags::kIsBypass as i32;
        } else {
            let param_hash = &self.param_hashes[param_index as usize];
            let default_value = &self.param_defaults_normalized[param_index as usize];
            let param_ptr = &self.param_by_hash[param_hash];

            info.id = *param_hash;
            u16strlcpy(&mut info.title, param_ptr.name());
            u16strlcpy(&mut info.short_title, param_ptr.name());
            u16strlcpy(&mut info.units, param_ptr.unit());
            // TODO: Don't forget this when we add enum parameters
            info.step_count = 0;
            info.default_normalized_value = *default_value as f64;
            info.unit_id = vst3_sys::vst::kRootUnitId;
            info.flags = vst3_sys::vst::ParameterFlags::kCanAutomate as i32;
        }

        kResultOk
    }

    unsafe fn get_param_string_by_value(
        &self,
        id: u32,
        value_normalized: f64,
        string: *mut TChar,
    ) -> tresult {
        check_null_ptr!(string);

        // Somehow there's no length there, so we'll assume our own maximum
        let dest = &mut *(string as *mut [TChar; 128]);

        if id == *BYPASS_PARAM_HASH {
            if value_normalized > 0.5 {
                u16strlcpy(dest, "Bypassed")
            } else {
                u16strlcpy(dest, "Enabled")
            }

            kResultOk
        } else if let Some(param_ptr) = self.param_by_hash.get(&id) {
            u16strlcpy(
                dest,
                &param_ptr.normalized_value_to_string(value_normalized as f32, false),
            );

            kResultOk
        } else {
            kInvalidArgument
        }
    }

    unsafe fn get_param_value_by_string(
        &self,
        id: u32,
        string: *const TChar,
        value_normalized: *mut f64,
    ) -> tresult {
        check_null_ptr!(string, value_normalized);

        let string = match U16CStr::from_ptr_str(string as *const u16).to_string() {
            Ok(s) => s,
            Err(_) => return kInvalidArgument,
        };

        if id == *BYPASS_PARAM_HASH {
            let value = match string.as_str() {
                "Bypassed" => 1.0,
                "Enabled" => 0.0,
                _ => return kResultFalse,
            };
            *value_normalized = value;

            kResultOk
        } else if let Some(param_ptr) = self.param_by_hash.get(&id) {
            let value = match param_ptr.string_to_normalized_value(&string) {
                Some(v) => v as f64,
                None => return kResultFalse,
            };
            *value_normalized = value;

            kResultOk
        } else {
            kInvalidArgument
        }
    }

    unsafe fn normalized_param_to_plain(&self, id: u32, value_normalized: f64) -> f64 {
        if id == *BYPASS_PARAM_HASH {
            value_normalized
        } else if let Some(param_ptr) = self.param_by_hash.get(&id) {
            param_ptr.preview_unnormalized(value_normalized as f32) as f64
        } else {
            0.5
        }
    }

    unsafe fn plain_param_to_normalized(&self, id: u32, plain_value: f64) -> f64 {
        if id == *BYPASS_PARAM_HASH {
            plain_value.clamp(0.0, 1.0)
        } else if let Some(param_ptr) = self.param_by_hash.get(&id) {
            param_ptr.preview_normalized(plain_value as f32) as f64
        } else {
            0.5
        }
    }

    unsafe fn get_param_normalized(&self, id: u32) -> f64 {
        if id == *BYPASS_PARAM_HASH {
            if self.bypass_state.get() {
                1.0
            } else {
                0.0
            }
        } else if let Some(param_ptr) = self.param_by_hash.get(&id) {
            param_ptr.normalized_value() as f64
        } else {
            0.5
        }
    }

    unsafe fn set_param_normalized(&self, id: u32, value: f64) -> tresult {
        // If the plugin is currently processing audio, then this parameter change will also be sent
        // to the process function
        if self.is_processing.load(Ordering::SeqCst) {
            return kResultOk;
        }

        self.set_normalized_value_by_hash(id, value)
    }

    unsafe fn set_component_handler(&self, _handler: *mut c_void) -> tresult {
        // TODO: Use this when we add GUI support
        kResultOk
    }

    unsafe fn create_view(&self, _name: vst3_sys::base::FIDString) -> *mut c_void {
        // We currently don't support GUIs
        std::ptr::null_mut()
    }
}

impl<P: Plugin> IAudioProcessor for Wrapper<'_, P> {
    unsafe fn set_bus_arrangements(
        &self,
        inputs: *mut vst3_sys::vst::SpeakerArrangement,
        num_ins: i32,
        outputs: *mut vst3_sys::vst::SpeakerArrangement,
        num_outs: i32,
    ) -> tresult {
        check_null_ptr!(inputs, outputs);

        // We currently only do single audio bus IO configurations
        if num_ins != 1 || num_outs != 1 {
            return kInvalidArgument;
        }

        let input_channel_map = &*inputs;
        let output_channel_map = &*outputs;
        let proposed_config = BusConfig {
            num_input_channels: input_channel_map.count_ones(),
            num_output_channels: output_channel_map.count_ones(),
        };
        if self.plugin.borrow().accepts_bus_config(&proposed_config) {
            self.current_bus_config.replace(proposed_config);

            kResultOk
        } else {
            kResultFalse
        }
    }

    unsafe fn get_bus_arrangement(
        &self,
        dir: vst3_sys::vst::BusDirection,
        index: i32,
        arr: *mut vst3_sys::vst::SpeakerArrangement,
    ) -> tresult {
        check_null_ptr!(arr);

        let config = self.current_bus_config.borrow();
        match (dir, index) {
            (d, 0) if d == vst3_sys::vst::BusDirections::kInput as i32 => {
                let channel_map = match config.num_input_channels {
                    0 => vst3_sys::vst::kEmpty,
                    1 => vst3_sys::vst::kMono,
                    2 => vst3_sys::vst::kStereo,
                    5 => vst3_sys::vst::k50,
                    6 => vst3_sys::vst::k51,
                    7 => vst3_sys::vst::k70Cine,
                    8 => vst3_sys::vst::k71Cine,
                    n => {
                        nih_debug_assert_failure!(
                            "No defined layout for {} channels, making something up on the spot...",
                            n
                        );
                        (1 << n) - 1
                    }
                };

                nih_debug_assert_eq!(config.num_input_channels, channel_map.count_ones());
                *arr = channel_map;

                kResultOk
            }
            _ => kInvalidArgument,
        }
    }

    unsafe fn can_process_sample_size(&self, symbolic_sample_size: i32) -> tresult {
        if symbolic_sample_size == vst3_sys::vst::SymbolicSampleSizes::kSample32 as i32 {
            kResultOk
        } else {
            kResultFalse
        }
    }

    unsafe fn get_latency_samples(&self) -> u32 {
        // TODO: Latency compensation
        0
    }

    unsafe fn setup_processing(&self, setup: *const vst3_sys::vst::ProcessSetup) -> tresult {
        check_null_ptr!(setup);

        // There's no special handling for offline processing at the moment
        let setup = &*setup;
        nih_debug_assert_eq!(
            setup.symbolic_sample_size,
            vst3_sys::vst::SymbolicSampleSizes::kSample32 as i32
        );

        let bus_config = self.current_bus_config.borrow();
        let buffer_config = BufferConfig {
            sample_rate: setup.sample_rate as f32,
            max_buffer_size: setup.max_samples_per_block as u32,
        };

        if self
            .plugin
            .borrow_mut()
            .initialize(&bus_config, &buffer_config)
        {
            // Preallocate enough room in the output slices vector so we can convert a `*mut *mut
            // f32` to a `&mut [&mut f32]` in the process call
            self.output_slices
                .borrow_mut()
                .resize_with(bus_config.num_output_channels as usize, || &mut []);

            kResultOk
        } else {
            kResultFalse
        }
    }

    unsafe fn set_processing(&self, state: TBool) -> tresult {
        // Always reset the processing status when the plugin gets activated or deactivated
        self.last_process_status.set(ProcessStatus::Normal);
        self.is_processing.store(state != 0, Ordering::SeqCst);

        // We don't have any special handling for suspending and resuming plugins, yet
        kResultOk
    }

    unsafe fn process(&self, data: *mut vst3_sys::vst::ProcessData) -> tresult {
        check_null_ptr!(data);

        // The setups we suppport are:
        // - 1 input bus
        // - 1 output bus
        // - 1 input bus and 1 output bus
        let data = &*data;
        nih_debug_assert!(
            data.num_inputs >= 0
                && data.num_inputs <= 1
                && data.num_outputs >= 0
                && data.num_outputs <= 1,
            "The host provides more than one input or output bus"
        );
        nih_debug_assert_eq!(
            data.symbolic_sample_size,
            vst3_sys::vst::SymbolicSampleSizes::kSample32 as i32
        );
        nih_debug_assert!(data.num_samples >= 0);
        if data.num_outputs < 1 {
            nih_debug_assert_failure!("The host doesn't provide any outputs");
            return kInvalidArgument;
        }

        // This vector has been reallocated to contain enough slices as there are output channels
        let mut output_slices = self.output_slices.borrow_mut();
        check_null_ptr_msg!(
            "Process output pointer is null",
            data.outputs,
            (*data.outputs).buffers,
        );

        let num_output_channels = (*data.outputs).num_channels as usize;
        nih_debug_assert_eq!(num_output_channels, output_slices.len());
        for (output_channel_idx, output_channel_slice) in output_slices.iter_mut().enumerate() {
            *output_channel_slice = std::slice::from_raw_parts_mut(
                *((*data.outputs).buffers as *mut *mut f32).add(output_channel_idx),
                data.num_samples as usize,
            );
        }

        // Most hosts process data in place, in which case we don't need to do any copying
        // ourselves. If the pointers do not alias, then we'll do the copy here and then the plugin
        // can just do normal in place processing.
        if !data.inputs.is_null() {
            let num_input_channels = (*data.inputs).num_channels as usize;
            nih_debug_assert!(
                num_input_channels <= num_output_channels,
                "Stereo to mono and similar configurations are not supported"
            );
            for input_channel_idx in 0..cmp::min(num_input_channels, num_output_channels) {
                let output_channel_ptr =
                    *((*data.outputs).buffers as *mut *mut f32).add(input_channel_idx);
                let input_channel_ptr =
                    *((*data.inputs).buffers as *const *const f32).add(input_channel_idx);
                if input_channel_ptr != output_channel_ptr {
                    ptr::copy_nonoverlapping(
                        input_channel_ptr,
                        output_channel_ptr,
                        data.num_samples as usize,
                    );
                }
            }
        }

        if let Some(param_changes) = data.input_param_changes.upgrade() {
            let num_param_queues = param_changes.get_parameter_count();
            for change_queue_idx in 0..num_param_queues {
                if let Some(param_change_queue) =
                    param_changes.get_parameter_data(change_queue_idx).upgrade()
                {
                    let param_hash = param_change_queue.get_parameter_id();
                    let num_changes = param_change_queue.get_point_count();

                    // TODO: Handle sample accurate parameter changes, possibly in a similar way to
                    //       the smoothing
                    let mut sample_offset = 0i32;
                    let mut value = 0.0f64;
                    if num_changes > 0
                        && param_change_queue.get_point(
                            num_changes - 1,
                            &mut sample_offset,
                            &mut value,
                        ) == kResultOk
                    {
                        self.set_normalized_value_by_hash(param_hash, value);
                    }
                }
            }
        }

        match self.plugin.borrow_mut().process(&mut output_slices) {
            ProcessStatus::Error(err) => {
                nih_debug_assert_failure!("Process error: {}", err);

                kResultFalse
            }
            _ => kResultOk,
        }
    }

    unsafe fn get_tail_samples(&self) -> u32 {
        // https://github.com/steinbergmedia/vst3_pluginterfaces/blob/2ad397ade5b51007860bedb3b01b8afd2c5f6fba/vst/ivstaudioprocessor.h#L145-L159
        match self.last_process_status.get() {
            ProcessStatus::Tail(samples) => samples,
            ProcessStatus::KeepAlive => u32::MAX, // kInfiniteTail
            _ => 0,                               // kNoTail
        }
    }
}

#[VST3(implements(IPluginFactory, IPluginFactory2, IPluginFactory3))]
pub struct Factory<P: Vst3Plugin> {
    /// The exposed plugin's GUID. Instead of generating this, we'll just let the programmer decide
    /// on their own.
    cid: GUID,
    /// The type will be used for constructing plugin instances later.
    _phantom: PhantomData<P>,
}

impl<P: Vst3Plugin> Factory<P> {
    pub fn new() -> Box<Self> {
        Self::allocate(
            GUID {
                data: P::VST3_CLASS_ID,
            },
            PhantomData::default(),
        )
    }
}

impl<P: Vst3Plugin> IPluginFactory for Factory<P> {
    unsafe fn get_factory_info(&self, info: *mut vst3_sys::base::PFactoryInfo) -> tresult {
        *info = mem::zeroed();

        let info = &mut *info;
        strlcpy(&mut info.vendor, P::VENDOR);
        strlcpy(&mut info.url, P::URL);
        strlcpy(&mut info.email, P::EMAIL);
        info.flags = vst3_sys::base::FactoryFlags::kUnicode as i32;

        kResultOk
    }

    unsafe fn count_classes(&self) -> i32 {
        // We don't do shell plugins, and good of an idea having separated components and edit
        // controllers in theory is, few software can use it, and doing that would make our simple
        // microframework a lot less simple
        1
    }

    unsafe fn get_class_info(&self, index: i32, info: *mut vst3_sys::base::PClassInfo) -> tresult {
        if index != 0 {
            return kInvalidArgument;
        }

        *info = mem::zeroed();

        let info = &mut *info;
        info.cid = self.cid;
        info.cardinality = vst3_sys::base::ClassCardinality::kManyInstances as i32;
        strlcpy(&mut info.category, "Audio Module Class");
        strlcpy(&mut info.name, P::NAME);

        kResultOk
    }

    unsafe fn create_instance(
        &self,
        cid: *const vst3_sys::IID,
        _iid: *const vst3_sys::IID,
        obj: *mut *mut vst3_sys::c_void,
    ) -> tresult {
        check_null_ptr!(cid, obj);

        if *cid != self.cid {
            return kNoInterface;
        }

        *obj = Box::into_raw(Wrapper::<P>::new()) as *mut vst3_sys::c_void;

        kResultOk
    }
}

impl<P: Vst3Plugin> IPluginFactory2 for Factory<P> {
    unsafe fn get_class_info2(
        &self,
        index: i32,
        info: *mut vst3_sys::base::PClassInfo2,
    ) -> tresult {
        if index != 0 {
            return kInvalidArgument;
        }

        *info = mem::zeroed();

        let info = &mut *info;
        info.cid = self.cid;
        info.cardinality = vst3_sys::base::ClassCardinality::kManyInstances as i32;
        strlcpy(&mut info.category, "Audio Module Class");
        strlcpy(&mut info.name, P::NAME);
        info.class_flags = 1 << 1; // kSimpleModeSupported
        strlcpy(&mut info.subcategories, P::VST3_CATEGORIES);
        strlcpy(&mut info.vendor, P::VENDOR);
        strlcpy(&mut info.version, P::VERSION);
        strlcpy(&mut info.sdk_version, VST3_SDK_VERSION);

        kResultOk
    }
}

impl<P: Vst3Plugin> IPluginFactory3 for Factory<P> {
    unsafe fn get_class_info_unicode(
        &self,
        index: i32,
        info: *mut vst3_sys::base::PClassInfoW,
    ) -> tresult {
        if index != 0 {
            return kInvalidArgument;
        }

        *info = mem::zeroed();

        let info = &mut *info;
        info.cid = self.cid;
        info.cardinality = vst3_sys::base::ClassCardinality::kManyInstances as i32;
        strlcpy(&mut info.category, "Audio Module Class");
        u16strlcpy(&mut info.name, P::NAME);
        info.class_flags = 1 << 1; // kSimpleModeSupported
        strlcpy(&mut info.subcategories, P::VST3_CATEGORIES);
        u16strlcpy(&mut info.vendor, P::VENDOR);
        u16strlcpy(&mut info.version, P::VERSION);
        u16strlcpy(&mut info.sdk_version, VST3_SDK_VERSION);

        kResultOk
    }

    unsafe fn set_host_context(&self, _context: *mut c_void) -> tresult {
        // We don't need to do anything with this
        kResultOk
    }
}

/// Export a VST3 plugin from this library using the provided plugin type.
///
/// TODO: Come up with some way to hae Cargo spit out a VST3 module. Is that possible without a
///       custom per-plugin build script?
#[macro_export]
macro_rules! nih_export_vst3 {
    ($plugin_ty:ty) => {
        #[no_mangle]
        pub extern "system" fn GetPluginFactory() -> *mut ::std::ffi::c_void {
            let factory = ::nih_plug::wrapper::vst3::Factory::<$plugin_ty>::new();

            Box::into_raw(factory) as *mut ::std::ffi::c_void
        }

        // We don't need any special initialization logic, so all of these module entry point
        // functions just return true all the time

        // These two entry points are used on Linux, and they would theoretically also be used on
        // the BSDs:
        // https://github.com/steinbergmedia/vst3_public_sdk/blob/c3948deb407bdbff89de8fb6ab8500ea4df9d6d9/source/main/linuxmain.cpp#L47-L52
        #[no_mangle]
        #[cfg(all(target_family = "unix", not(target_os = "macos")))]
        pub extern "C" fn ModuleEntry(_lib_handle: *mut ::std::ffi::c_void) -> bool {
            true
        }

        #[no_mangle]
        #[cfg(all(target_family = "unix", not(target_os = "macos")))]
        pub extern "C" fn ModuleExit() -> bool {
            true
        }

        // These two entry points are used on macOS:
        // https://github.com/steinbergmedia/vst3_public_sdk/blob/bc459feee68803346737901471441fd4829ec3f9/source/main/macmain.cpp#L60-L61
        #[no_mangle]
        #[cfg(target_os = "macos")]
        pub extern "C" fn bundleEntry(_lib_handle: *mut ::std::ffi::c_void) -> bool {
            true
        }

        #[no_mangle]
        #[cfg(target_os = "macos")]
        pub extern "C" fn bundleExit() -> bool {
            true
        }

        // And these two entry points are used on Windows:
        // https://github.com/steinbergmedia/vst3_public_sdk/blob/bc459feee68803346737901471441fd4829ec3f9/source/main/dllmain.cpp#L59-L60
        #[no_mangle]
        #[cfg(target_os = "windows")]
        pub extern "system" fn InitModule() -> bool {
            true
        }

        #[no_mangle]
        #[cfg(target_os = "windows")]
        pub extern "system" fn DeinitModule() -> bool {
            true
        }
    };
}