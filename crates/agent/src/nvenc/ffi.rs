//! nvEncodeAPI.h(nv-codec-headers n13.0.19.0,MIT 式许可)的最小子集。
//!
//! 只声明用到的字段,其余用字节数组补齐。尺寸和偏移来自 MSVC x64 实测,
//! 由文件末尾的编译期断言保证一致。

#![allow(non_camel_case_types, dead_code)]

use std::ffi::{c_char, c_void};
use std::mem::{offset_of, size_of};

use windows::core::GUID;

pub type NVENCSTATUS = i32;
pub const NV_ENC_SUCCESS: NVENCSTATUS = 0;

pub const NVENCAPI_MAJOR_VERSION: u32 = 13;
pub const NVENCAPI_MINOR_VERSION: u32 = 0;
pub const NVENCAPI_VERSION: u32 = NVENCAPI_MAJOR_VERSION | (NVENCAPI_MINOR_VERSION << 24);

const fn struct_ver(v: u32) -> u32 {
    NVENCAPI_VERSION | (v << 16) | (0x7 << 28)
}

pub const NV_ENCODE_API_FUNCTION_LIST_VER: u32 = struct_ver(2);
pub const NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER: u32 = struct_ver(1);
pub const NV_ENC_RC_PARAMS_VER: u32 = struct_ver(1);
pub const NV_ENC_CONFIG_VER: u32 = struct_ver(9) | (1 << 31);
pub const NV_ENC_INITIALIZE_PARAMS_VER: u32 = struct_ver(7) | (1 << 31);
pub const NV_ENC_PRESET_CONFIG_VER: u32 = struct_ver(5) | (1 << 31);
pub const NV_ENC_PIC_PARAMS_VER: u32 = struct_ver(7) | (1 << 31);
pub const NV_ENC_LOCK_BITSTREAM_VER: u32 = struct_ver(2) | (1 << 31);
pub const NV_ENC_CREATE_BITSTREAM_BUFFER_VER: u32 = struct_ver(1);
pub const NV_ENC_MAP_INPUT_RESOURCE_VER: u32 = struct_ver(4);
pub const NV_ENC_REGISTER_RESOURCE_VER: u32 = struct_ver(5);

pub const NV_ENC_DEVICE_TYPE_DIRECTX: u32 = 0;
pub const NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX: u32 = 0;
pub const NV_ENC_INPUT_IMAGE: u32 = 0;
/// DXGI_FORMAT_B8G8R8A8_UNORM 对应 NVENC 的 ARGB(字节序 B,G,R,A)
pub const NV_ENC_BUFFER_FORMAT_ARGB: u32 = 0x0100_0000;
pub const NV_ENC_PIC_STRUCT_FRAME: u32 = 1;
pub const NV_ENC_PARAMS_RC_CBR: u32 = 2;
pub const NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY: u32 = 3;
pub const NV_ENC_PIC_FLAG_FORCEIDR: u32 = 0x2;
pub const NV_ENC_PIC_FLAG_OUTPUT_SPSPPS: u32 = 0x4;
pub const NV_ENC_INFINITE_GOPLENGTH: u32 = 0xffff_ffff;
/// NV_ENC_CONFIG_H264 第一个位域字里 repeatSPSPPS 的位置
pub const H264_REPEAT_SPSPPS_BIT: u32 = 1 << 12;

pub const NV_ENC_CODEC_H264_GUID: GUID = GUID::from_u128(0x6bc82762_4e63_4ca4_aa85_1e50f321f6bf);
pub const NV_ENC_H264_PROFILE_BASELINE_GUID: GUID =
    GUID::from_u128(0x0727bcaa_78c4_4c83_8c2f_ef3dff267c6a);
pub const NV_ENC_H264_PROFILE_HIGH_GUID: GUID =
    GUID::from_u128(0xe7cbc309_4f7a_4b89_af2a_d537c92be310);
pub const NV_ENC_PRESET_P4_GUID: GUID = GUID::from_u128(0x90a7b826_df06_4862_b9d2_cd6d73a08681);

#[repr(C)]
pub struct NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
    pub version: u32,
    pub device_type: u32,
    pub device: *mut c_void,
    pub reserved: *mut c_void,
    pub api_version: u32,
    _rest: [u8; 1552 - 28],
}

#[repr(C)]
pub struct NV_ENC_RC_PARAMS {
    pub version: u32,
    pub rate_control_mode: u32,
    pub const_qp: [u32; 3],
    pub average_bit_rate: u32,
    pub max_bit_rate: u32,
    pub vbv_buffer_size: u32,
    pub vbv_initial_delay: u32,
    _rest: [u8; 128 - 36],
}

/// NV_ENC_CODEC_CONFIG 联合体,H.264 视图只用到开头几个字段
#[repr(C, align(8))]
pub struct NV_ENC_CODEC_CONFIG {
    pub h264_flags: u32,
    pub h264_level: u32,
    pub h264_idr_period: u32,
    _rest: [u8; 1792 - 12],
}

#[repr(C)]
pub struct NV_ENC_CONFIG {
    pub version: u32,
    pub profile_guid: GUID,
    pub gop_length: u32,
    pub frame_interval_p: i32,
    pub mono_chrome_encoding: u32,
    pub frame_field_mode: u32,
    pub mv_precision: u32,
    pub rc_params: NV_ENC_RC_PARAMS,
    pub encode_codec_config: NV_ENC_CODEC_CONFIG,
    _rest: [u8; 3584 - 1960],
}

#[repr(C)]
pub struct NV_ENC_PRESET_CONFIG {
    pub version: u32,
    pub reserved: u32,
    pub preset_cfg: NV_ENC_CONFIG,
    _rest: [u8; 5128 - 3592],
}
#[repr(C)]
pub struct NV_ENC_INITIALIZE_PARAMS {
    pub version: u32,
    pub encode_guid: GUID,
    pub preset_guid: GUID,
    pub encode_width: u32,
    pub encode_height: u32,
    pub dar_width: u32,
    pub dar_height: u32,
    pub frame_rate_num: u32,
    pub frame_rate_den: u32,
    pub enable_encode_async: u32,
    pub enable_ptd: u32,
    pub flags: u32,
    pub priv_data_size: u32,
    pub priv_data: *mut c_void,
    pub encode_config: *mut NV_ENC_CONFIG,
    pub max_encode_width: u32,
    pub max_encode_height: u32,
    pub max_me_hint_counts_per_block: [u8; 32],
    pub tuning_info: u32,
    pub buffer_format: u32,
    _rest: [u8; 1800 - 144],
}

#[repr(C)]
pub struct NV_ENC_CREATE_BITSTREAM_BUFFER {
    pub version: u32,
    pub size: u32,
    pub memory_heap: u32,
    pub reserved: u32,
    pub bitstream_buffer: *mut c_void,
    _rest: [u8; 776 - 24],
}

#[repr(C)]
pub struct NV_ENC_REGISTER_RESOURCE {
    pub version: u32,
    pub resource_type: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub sub_resource_index: u32,
    pub resource_to_register: *mut c_void,
    pub registered_resource: *mut c_void,
    pub buffer_format: u32,
    pub buffer_usage: u32,
    pub input_fence_point: *mut c_void,
    _rest: [u8; 1536 - 56],
}

#[repr(C)]
pub struct NV_ENC_MAP_INPUT_RESOURCE {
    pub version: u32,
    pub sub_resource_index: u32,
    pub input_resource: *mut c_void,
    pub registered_resource: *mut c_void,
    pub mapped_resource: *mut c_void,
    pub mapped_buffer_fmt: u32,
    _rest: [u8; 1544 - 36],
}

#[repr(C)]
pub struct NV_ENC_PIC_PARAMS {
    pub version: u32,
    pub input_width: u32,
    pub input_height: u32,
    pub input_pitch: u32,
    pub encode_pic_flags: u32,
    pub frame_idx: u32,
    pub input_time_stamp: u64,
    pub input_duration: u64,
    pub input_buffer: *mut c_void,
    pub output_bitstream: *mut c_void,
    pub completion_event: *mut c_void,
    pub buffer_fmt: u32,
    pub picture_struct: u32,
    pub picture_type: u32,
    _rest: [u8; 3360 - 76],
}

#[repr(C)]
pub struct NV_ENC_LOCK_BITSTREAM {
    pub version: u32,
    pub flags: u32,
    pub output_bitstream: *mut c_void,
    pub slice_offsets: *mut u32,
    pub frame_idx: u32,
    pub hw_encode_status: u32,
    pub num_slices: u32,
    pub bitstream_size_in_bytes: u32,
    pub output_time_stamp: u64,
    pub output_duration: u64,
    pub bitstream_buffer_ptr: *mut c_void,
    pub picture_type: u32,
    _rest: [u8; 1544 - 68],
}

/// NV_ENCODE_API_FUNCTION_LIST:两个 u32 之后是 43 个函数指针和 275 个保留指针。
/// 按下标取用,下标常量见 `fnidx`。
#[repr(C)]
pub struct NV_ENCODE_API_FUNCTION_LIST {
    pub version: u32,
    pub reserved: u32,
    pub fns: [*mut c_void; 43 + 275],
}

pub mod fnidx {
    pub const INITIALIZE_ENCODER: usize = 11;
    pub const CREATE_BITSTREAM_BUFFER: usize = 14;
    pub const DESTROY_BITSTREAM_BUFFER: usize = 15;
    pub const ENCODE_PICTURE: usize = 16;
    pub const LOCK_BITSTREAM: usize = 17;
    pub const UNLOCK_BITSTREAM: usize = 18;
    pub const MAP_INPUT_RESOURCE: usize = 25;
    pub const UNMAP_INPUT_RESOURCE: usize = 26;
    pub const DESTROY_ENCODER: usize = 27;
    pub const OPEN_ENCODE_SESSION_EX: usize = 29;
    pub const REGISTER_RESOURCE: usize = 30;
    pub const UNREGISTER_RESOURCE: usize = 31;
    pub const GET_LAST_ERROR_STRING: usize = 37;
    pub const GET_ENCODE_PRESET_CONFIG_EX: usize = 39;
}

pub type PfnGetMaxSupportedVersion = unsafe extern "system" fn(*mut u32) -> NVENCSTATUS;
pub type PfnCreateInstance =
    unsafe extern "system" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;
pub type PfnOpenEncodeSessionEx = unsafe extern "system" fn(
    *mut NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS,
    *mut *mut c_void,
) -> NVENCSTATUS;
pub type PfnGetEncodePresetConfigEx = unsafe extern "system" fn(
    *mut c_void,
    GUID,
    GUID,
    u32,
    *mut NV_ENC_PRESET_CONFIG,
) -> NVENCSTATUS;
pub type PfnInitializeEncoder =
    unsafe extern "system" fn(*mut c_void, *mut NV_ENC_INITIALIZE_PARAMS) -> NVENCSTATUS;
pub type PfnCreateBitstreamBuffer =
    unsafe extern "system" fn(*mut c_void, *mut NV_ENC_CREATE_BITSTREAM_BUFFER) -> NVENCSTATUS;
pub type PfnRegisterResource =
    unsafe extern "system" fn(*mut c_void, *mut NV_ENC_REGISTER_RESOURCE) -> NVENCSTATUS;
pub type PfnMapInputResource =
    unsafe extern "system" fn(*mut c_void, *mut NV_ENC_MAP_INPUT_RESOURCE) -> NVENCSTATUS;
pub type PfnEncodePicture =
    unsafe extern "system" fn(*mut c_void, *mut NV_ENC_PIC_PARAMS) -> NVENCSTATUS;
pub type PfnLockBitstream =
    unsafe extern "system" fn(*mut c_void, *mut NV_ENC_LOCK_BITSTREAM) -> NVENCSTATUS;
/// Destroy/Unregister/Unmap/Unlock 都是 (encoder, handle) 形式
pub type PfnHandle = unsafe extern "system" fn(*mut c_void, *mut c_void) -> NVENCSTATUS;
pub type PfnDestroyEncoder = unsafe extern "system" fn(*mut c_void) -> NVENCSTATUS;
pub type PfnGetLastErrorString = unsafe extern "system" fn(*mut c_void) -> *const c_char;

// 与 MSVC x64 实测结果对照
const _: () = {
    assert!(size_of::<NV_ENCODE_API_FUNCTION_LIST>() == 2552);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, fns) + 8 * fnidx::OPEN_ENCODE_SESSION_EX == 240);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, fns) + 8 * fnidx::GET_LAST_ERROR_STRING == 304);
    assert!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, fns) + 8 * fnidx::GET_ENCODE_PRESET_CONFIG_EX
            == 320
    );

    assert!(size_of::<NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS>() == 1552);
    assert!(offset_of!(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, device) == 8);
    assert!(offset_of!(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, api_version) == 24);

    assert!(size_of::<NV_ENC_RC_PARAMS>() == 128);
    assert!(offset_of!(NV_ENC_RC_PARAMS, rate_control_mode) == 4);
    assert!(offset_of!(NV_ENC_RC_PARAMS, average_bit_rate) == 20);
    assert!(offset_of!(NV_ENC_RC_PARAMS, vbv_initial_delay) == 32);

    assert!(size_of::<NV_ENC_CODEC_CONFIG>() == 1792);
    assert!(offset_of!(NV_ENC_CODEC_CONFIG, h264_idr_period) == 8);

    assert!(size_of::<NV_ENC_CONFIG>() == 3584);
    assert!(offset_of!(NV_ENC_CONFIG, profile_guid) == 4);
    assert!(offset_of!(NV_ENC_CONFIG, gop_length) == 20);
    assert!(offset_of!(NV_ENC_CONFIG, frame_interval_p) == 24);
    assert!(offset_of!(NV_ENC_CONFIG, mv_precision) == 36);
    assert!(offset_of!(NV_ENC_CONFIG, rc_params) == 40);
    assert!(offset_of!(NV_ENC_CONFIG, encode_codec_config) == 168);
    assert!(offset_of!(NV_ENC_CONFIG, _rest) == 1960);

    assert!(size_of::<NV_ENC_PRESET_CONFIG>() == 5128);
    assert!(offset_of!(NV_ENC_PRESET_CONFIG, preset_cfg) == 8);

    assert!(size_of::<NV_ENC_INITIALIZE_PARAMS>() == 1800);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, encode_guid) == 4);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, preset_guid) == 20);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, encode_width) == 36);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, enable_encode_async) == 60);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, enable_ptd) == 64);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, priv_data_size) == 72);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, encode_config) == 88);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, max_encode_width) == 96);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, max_me_hint_counts_per_block) == 104);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, tuning_info) == 136);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, buffer_format) == 140);

    assert!(size_of::<NV_ENC_CREATE_BITSTREAM_BUFFER>() == 776);
    assert!(offset_of!(NV_ENC_CREATE_BITSTREAM_BUFFER, bitstream_buffer) == 16);

    assert!(size_of::<NV_ENC_REGISTER_RESOURCE>() == 1536);
    assert!(offset_of!(NV_ENC_REGISTER_RESOURCE, width) == 8);
    assert!(offset_of!(NV_ENC_REGISTER_RESOURCE, sub_resource_index) == 20);
    assert!(offset_of!(NV_ENC_REGISTER_RESOURCE, resource_to_register) == 24);
    assert!(offset_of!(NV_ENC_REGISTER_RESOURCE, registered_resource) == 32);
    assert!(offset_of!(NV_ENC_REGISTER_RESOURCE, buffer_format) == 40);
    assert!(offset_of!(NV_ENC_REGISTER_RESOURCE, buffer_usage) == 44);
    assert!(offset_of!(NV_ENC_REGISTER_RESOURCE, input_fence_point) == 48);

    assert!(size_of::<NV_ENC_MAP_INPUT_RESOURCE>() == 1544);
    assert!(offset_of!(NV_ENC_MAP_INPUT_RESOURCE, input_resource) == 8);
    assert!(offset_of!(NV_ENC_MAP_INPUT_RESOURCE, registered_resource) == 16);
    assert!(offset_of!(NV_ENC_MAP_INPUT_RESOURCE, mapped_resource) == 24);
    assert!(offset_of!(NV_ENC_MAP_INPUT_RESOURCE, mapped_buffer_fmt) == 32);

    assert!(size_of::<NV_ENC_PIC_PARAMS>() == 3360);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, input_time_stamp) == 24);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, input_buffer) == 40);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, output_bitstream) == 48);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, completion_event) == 56);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, buffer_fmt) == 64);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, picture_type) == 72);

    assert!(size_of::<NV_ENC_LOCK_BITSTREAM>() == 1544);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, output_bitstream) == 8);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, frame_idx) == 24);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, bitstream_size_in_bytes) == 36);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, output_time_stamp) == 40);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, bitstream_buffer_ptr) == 56);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, picture_type) == 64);
};
