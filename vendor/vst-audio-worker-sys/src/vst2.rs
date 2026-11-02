// SPDX-License-Identifier: BSD-3-Clause
// ABI translated from Xaymar/vst2sdk include/vst.h at
// 339d4f31590bf77c0d0d248e09a380ac6285e069.
// Copyright (c) 2020 Xaymar Dirks <info@xaymar.com> (previously known as Michael Fabian Dirks)
// Copyright 2020 Michael Fabian 'Xaymar' Dirks <info@xaymar.com>
//
// Redistribution and use in source and binary forms, with or without modification, are permitted provided that the following conditions are met:
//
// 1. Redistributions of source code must retain the above copyright notice, this list of conditions and the following disclaimer.
//
// 2. Redistributions in binary form must reproduce the above copyright notice, this list of conditions and the following disclaimer in the documentation and/or other materials provided with the distribution.
//
// 3. Neither the name of the copyright holder nor the names of its contributors may be used to endorse or promote products derived from this software without specific prior written permission.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

#![allow(unsafe_code)]
use std::ffi::c_void;

pub const MAGIC: i32 = i32::from_be_bytes(*b"VstP");
pub const HAS_EDITOR: i32 = 1;
pub const REPLACING: i32 = 1 << 4;
pub const DOUBLE_REPLACING: i32 = 1 << 12;
pub const PROGRAM_CHUNKS: i32 = 1 << 5;
pub const OPEN: i32 = 0;
pub const CLOSE: i32 = 1;
pub const SET_PROGRAM: i32 = 2;
pub const GET_PROGRAM: i32 = 3;
pub const PARAM_NAME: i32 = 8;
pub const SET_SAMPLE_RATE: i32 = 10;
pub const SET_BLOCK_SIZE: i32 = 11;
pub const MAINS_CHANGED: i32 = 12;
pub const EDIT_GET_RECT: i32 = 13;
pub const EDIT_OPEN: i32 = 14;
pub const EDIT_CLOSE: i32 = 15;
pub const EDIT_IDLE: i32 = 19;
pub const GET_CHUNK: i32 = 23;
pub const SET_CHUNK: i32 = 24;
pub const SET_SPEAKER_ARRANGEMENT: i32 = 42;
pub const GET_EFFECT_NAME: i32 = 45;
pub const GET_VENDOR_STRING: i32 = 47;
pub const GET_SPEAKER_ARRANGEMENT: i32 = 69;
pub const START_PROCESS: i32 = 71;
pub const STOP_PROCESS: i32 = 72;
pub const MASTER_AUTOMATE: i32 = 0;
pub const MASTER_VERSION: i32 = 1;
pub const MASTER_IO_CHANGED: i32 = 13;
pub const MASTER_SIZE_WINDOW: i32 = 15;
pub const MASTER_SAMPLE_RATE: i32 = 16;
pub const MASTER_BLOCK_SIZE: i32 = 17;
pub const MASTER_VENDOR: i32 = 32;
pub const MASTER_PRODUCT: i32 = 33;
pub const ARR_STEREO: i32 = 1;
pub const ARR_51: i32 = 15;

pub type Callback = unsafe extern "C" fn(*mut AEffect, i32, i32, isize, *mut c_void, f32) -> isize;
pub type Process = unsafe extern "C" fn(*mut AEffect, *const *const f32, *mut *mut f32, i32);
pub type Process64 = unsafe extern "C" fn(*mut AEffect, *const *const f64, *mut *mut f64, i32);
pub type SetParameter = unsafe extern "C" fn(*mut AEffect, u32, f32);
pub type GetParameter = unsafe extern "C" fn(*mut AEffect, u32) -> f32;
pub type Entry = unsafe extern "C" fn(Callback) -> *mut AEffect;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct AEffect {
    pub magic: i32,
    pub dispatcher: Option<Callback>,
    pub process: Option<Process>,
    pub set_parameter: Option<SetParameter>,
    pub get_parameter: Option<GetParameter>,
    pub num_programs: i32,
    pub num_params: i32,
    pub num_inputs: i32,
    pub num_outputs: i32,
    pub flags: i32,
    pub resvd1: *mut c_void,
    pub resvd2: *mut c_void,
    pub initial_delay: i32,
    pub real_qualities: i32,
    pub off_qualities: i32,
    pub io_ratio: f32,
    pub object: *mut c_void,
    pub user: *mut c_void,
    pub unique_id: i32,
    pub version: i32,
    pub process_replacing: Option<Process>,
    pub process_double: Option<Process64>,
    pub future: [u8; 56],
}
impl Default for AEffect {
    fn default() -> Self {
        Self {
            magic: MAGIC,
            dispatcher: None,
            process: None,
            set_parameter: None,
            get_parameter: None,
            num_programs: 0,
            num_params: 0,
            num_inputs: 0,
            num_outputs: 0,
            flags: 0,
            resvd1: std::ptr::null_mut(),
            resvd2: std::ptr::null_mut(),
            initial_delay: 0,
            real_qualities: 0,
            off_qualities: 0,
            io_ratio: 1.0,
            object: std::ptr::null_mut(),
            user: std::ptr::null_mut(),
            unique_id: 0,
            version: 0,
            process_replacing: None,
            process_double: None,
            future: [0; 56],
        }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Speaker {
    pub azimuth: f32,
    pub elevation: f32,
    pub radius: f32,
    pub reserved: f32,
    pub name: [u8; 64],
    pub kind: i32,
    pub future: [u8; 28],
}
impl Default for Speaker {
    fn default() -> Self {
        Self {
            azimuth: 0.0,
            elevation: 0.0,
            radius: 0.0,
            reserved: 0.0,
            name: [0; 64],
            kind: 0,
            future: [0; 28],
        }
    }
}
#[repr(C)]
#[derive(Clone, Debug)]
pub struct Arrangement {
    pub kind: i32,
    pub channels: i32,
    pub speakers: [Speaker; 32],
}
impl Arrangement {
    pub fn new(kind: i32, types: &[i32]) -> Self {
        assert!(types.len() <= 32);
        let mut result = Self {
            kind,
            channels: types.len() as i32,
            speakers: [Speaker::default(); 32],
        };
        for (speaker, kind) in result.speakers.iter_mut().zip(types) {
            speaker.kind = *kind;
        }
        result
    }
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Rect {
    pub top: i16,
    pub left: i16,
    pub bottom: i16,
    pub right: i16,
}
