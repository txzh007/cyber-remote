//! NVENC H.264 编码:D3D11 纹理输入,同步模式,输出 Annex-B。

mod ffi;

use std::ffi::{CStr, c_void};
use std::mem::{transmute, zeroed};
use std::ptr::null_mut;

use anyhow::{Result, anyhow, bail};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::{Interface, s, w};

use ffi::*;

/// 从 nvEncodeAPI64.dll 取到的函数表。DLL 加载后不卸载。
#[derive(Clone, Copy)]
pub struct Api {
    open_session_ex: PfnOpenEncodeSessionEx,
    get_preset_config_ex: PfnGetEncodePresetConfigEx,
    initialize: PfnInitializeEncoder,
    create_bitstream: PfnCreateBitstreamBuffer,
    destroy_bitstream: PfnHandle,
    register: PfnRegisterResource,
    unregister: PfnHandle,
    map: PfnMapInputResource,
    unmap: PfnHandle,
    encode: PfnEncodePicture,
    lock: PfnLockBitstream,
    unlock: PfnHandle,
    destroy: PfnDestroyEncoder,
    last_error: PfnGetLastErrorString,
}

impl Api {
    pub fn load() -> Result<Self> {
        unsafe {
            let lib = LoadLibraryW(w!("nvEncodeAPI64.dll"))
                .map_err(|e| anyhow!("加载 nvEncodeAPI64.dll 失败: {e}"))?;
            let max_ver: PfnGetMaxSupportedVersion = transmute(
                GetProcAddress(lib, s!("NvEncodeAPIGetMaxSupportedVersion"))
                    .ok_or_else(|| anyhow!("缺少 NvEncodeAPIGetMaxSupportedVersion"))?,
            );
            let create: PfnCreateInstance = transmute(
                GetProcAddress(lib, s!("NvEncodeAPICreateInstance"))
                    .ok_or_else(|| anyhow!("缺少 NvEncodeAPICreateInstance"))?,
            );

            let mut ver = 0u32;
            check(max_ver(&mut ver), "NvEncodeAPIGetMaxSupportedVersion")?;
            let (major, minor) = (ver >> 4, ver & 0xf);
            if (major, minor) < (NVENCAPI_MAJOR_VERSION, NVENCAPI_MINOR_VERSION) {
                bail!(
                    "驱动只支持 NVENC API {major}.{minor},需要 {NVENCAPI_MAJOR_VERSION}.{NVENCAPI_MINOR_VERSION},请升级 NVIDIA 驱动"
                );
            }
            tracing::info!("NVENC API 驱动支持 {major}.{minor}");

            let mut list: Box<NV_ENCODE_API_FUNCTION_LIST> = Box::new(zeroed());
            list.version = NV_ENCODE_API_FUNCTION_LIST_VER;
            check(create(&mut *list), "NvEncodeAPICreateInstance")?;

            let f = |i: usize| -> Result<*mut c_void> {
                let p = list.fns[i];
                if p.is_null() {
                    bail!("NVENC 函数表第 {i} 项为空");
                }
                Ok(p)
            };
            Ok(Self {
                open_session_ex: transmute(f(fnidx::OPEN_ENCODE_SESSION_EX)?),
                get_preset_config_ex: transmute(f(fnidx::GET_ENCODE_PRESET_CONFIG_EX)?),
                initialize: transmute(f(fnidx::INITIALIZE_ENCODER)?),
                create_bitstream: transmute(f(fnidx::CREATE_BITSTREAM_BUFFER)?),
                destroy_bitstream: transmute(f(fnidx::DESTROY_BITSTREAM_BUFFER)?),
                register: transmute(f(fnidx::REGISTER_RESOURCE)?),
                unregister: transmute(f(fnidx::UNREGISTER_RESOURCE)?),
                map: transmute(f(fnidx::MAP_INPUT_RESOURCE)?),
                unmap: transmute(f(fnidx::UNMAP_INPUT_RESOURCE)?),
                encode: transmute(f(fnidx::ENCODE_PICTURE)?),
                lock: transmute(f(fnidx::LOCK_BITSTREAM)?),
                unlock: transmute(f(fnidx::UNLOCK_BITSTREAM)?),
                destroy: transmute(f(fnidx::DESTROY_ENCODER)?),
                last_error: transmute(f(fnidx::GET_LAST_ERROR_STRING)?),
            })
        }
    }
}

fn check(status: NVENCSTATUS, what: &str) -> Result<()> {
    if status == NV_ENC_SUCCESS {
        Ok(())
    } else {
        Err(anyhow!("{what} 失败,NVENCSTATUS = {status}"))
    }
}

pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
}

/// 绑定到一张固定纹理的编码器。纹理尺寸变化时重建编码器。
pub struct Encoder {
    api: Api,
    enc: *mut c_void,
    bitstream: *mut c_void,
    registered: *mut c_void,
    width: u32,
    height: u32,
    frame_idx: u32,
    // 持有引用,保证注册期间设备和纹理存活
    _device: ID3D11Device,
    _texture: ID3D11Texture2D,
}

// 编码器只在采集线程里使用,这里允许它随线程移动
unsafe impl Send for Encoder {}
impl Encoder {
    /// `texture` 必须是 DXGI_FORMAT_B8G8R8A8_UNORM、尺寸等于 `cfg.width × cfg.height`,
    /// 且属于 `device`。
    pub fn new(
        api: &Api,
        device: &ID3D11Device,
        texture: &ID3D11Texture2D,
        cfg: &EncoderConfig,
    ) -> Result<Self> {
        let mut this = Self {
            api: *api,
            enc: null_mut(),
            bitstream: null_mut(),
            registered: null_mut(),
            width: cfg.width,
            height: cfg.height,
            frame_idx: 0,
            _device: device.clone(),
            _texture: texture.clone(),
        };
        // 失败时 Drop 会释放已创建的部分
        unsafe { this.init(cfg)? };
        Ok(this)
    }

    unsafe fn init(&mut self, cfg: &EncoderConfig) -> Result<()> {
        unsafe {
            let mut open: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = zeroed();
            open.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
            open.device_type = NV_ENC_DEVICE_TYPE_DIRECTX;
            open.device = self._device.as_raw();
            open.api_version = NVENCAPI_VERSION;
            check(
                (self.api.open_session_ex)(&mut open, &mut self.enc),
                "nvEncOpenEncodeSessionEx",
            )?;

            let mut preset: Box<NV_ENC_PRESET_CONFIG> = Box::new(zeroed());
            preset.version = NV_ENC_PRESET_CONFIG_VER;
            preset.preset_cfg.version = NV_ENC_CONFIG_VER;
            self.call(
                (self.api.get_preset_config_ex)(
                    self.enc,
                    NV_ENC_CODEC_H264_GUID,
                    NV_ENC_PRESET_P4_GUID,
                    NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                    &mut *preset,
                ),
                "nvEncGetEncodePresetConfigEx",
            )?;

            let config = &mut preset.preset_cfg;
            config.version = NV_ENC_CONFIG_VER;
            // SDP 里协商的是 profile-level-id=640032(High);High 有 CABAC 和 8x8 变换,
            // 同码率下文字边缘比 Baseline 清楚
            config.profile_guid = NV_ENC_H264_PROFILE_HIGH_GUID;
            config.gop_length = NV_ENC_INFINITE_GOPLENGTH;
            config.frame_interval_p = 1;
            let h264 = &mut config.encode_codec_config;
            h264.h264_idr_period = NV_ENC_INFINITE_GOPLENGTH;
            h264.h264_flags |= H264_REPEAT_SPSPPS_BIT;
            let rc = &mut config.rc_params;
            rc.version = NV_ENC_RC_PARAMS_VER;
            rc.rate_control_mode = NV_ENC_PARAMS_RC_CBR;
            rc.average_bit_rate = cfg.bitrate;
            rc.max_bit_rate = cfg.bitrate;
            // 4 帧 VBV:单帧 VBV 下 IDR 和画面突变只能压得很糊;多 3 帧缓冲约多 50ms 突发延迟
            rc.vbv_buffer_size = cfg.bitrate / cfg.fps * 4;
            rc.vbv_initial_delay = rc.vbv_buffer_size;

            let mut init: Box<NV_ENC_INITIALIZE_PARAMS> = Box::new(zeroed());
            init.version = NV_ENC_INITIALIZE_PARAMS_VER;
            init.encode_guid = NV_ENC_CODEC_H264_GUID;
            init.preset_guid = NV_ENC_PRESET_P4_GUID;
            init.encode_width = cfg.width;
            init.encode_height = cfg.height;
            init.dar_width = cfg.width;
            init.dar_height = cfg.height;
            init.frame_rate_num = cfg.fps;
            init.frame_rate_den = 1;
            init.enable_encode_async = 0;
            init.enable_ptd = 1;
            init.encode_config = config;
            init.tuning_info = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
            init.buffer_format = NV_ENC_BUFFER_FORMAT_ARGB;
            self.call(
                (self.api.initialize)(self.enc, &mut *init),
                "nvEncInitializeEncoder",
            )?;

            let mut bs: NV_ENC_CREATE_BITSTREAM_BUFFER = zeroed();
            bs.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
            self.call(
                (self.api.create_bitstream)(self.enc, &mut bs),
                "nvEncCreateBitstreamBuffer",
            )?;
            self.bitstream = bs.bitstream_buffer;

            let mut reg: NV_ENC_REGISTER_RESOURCE = zeroed();
            reg.version = NV_ENC_REGISTER_RESOURCE_VER;
            reg.resource_type = NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
            reg.width = cfg.width;
            reg.height = cfg.height;
            reg.resource_to_register = self._texture.as_raw();
            reg.buffer_format = NV_ENC_BUFFER_FORMAT_ARGB;
            reg.buffer_usage = NV_ENC_INPUT_IMAGE;
            self.call(
                (self.api.register)(self.enc, &mut reg),
                "nvEncRegisterResource",
            )?;
            self.registered = reg.registered_resource;
        }
        Ok(())
    }

    /// 编码纹理当前内容,返回一帧 Annex-B 数据。
    pub fn encode(&mut self, force_idr: bool, timestamp: u64) -> Result<Vec<u8>> {
        unsafe {
            let mut map: NV_ENC_MAP_INPUT_RESOURCE = zeroed();
            map.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
            map.registered_resource = self.registered;
            self.call(
                (self.api.map)(self.enc, &mut map),
                "nvEncMapInputResource",
            )?;

            let result = self.encode_mapped(&map, force_idr, timestamp);
            let unmap = (self.api.unmap)(self.enc, map.mapped_resource);
            let data = result?;
            self.call(unmap, "nvEncUnmapInputResource")?;
            Ok(data)
        }
    }

    unsafe fn encode_mapped(
        &mut self,
        map: &NV_ENC_MAP_INPUT_RESOURCE,
        force_idr: bool,
        timestamp: u64,
    ) -> Result<Vec<u8>> {
        unsafe {
            let mut pic: Box<NV_ENC_PIC_PARAMS> = Box::new(zeroed());
            pic.version = NV_ENC_PIC_PARAMS_VER;
            pic.input_width = self.width;
            pic.input_height = self.height;
            pic.frame_idx = self.frame_idx;
            pic.input_time_stamp = timestamp;
            pic.input_buffer = map.mapped_resource;
            pic.output_bitstream = self.bitstream;
            pic.buffer_fmt = map.mapped_buffer_fmt;
            pic.picture_struct = NV_ENC_PIC_STRUCT_FRAME;
            if force_idr {
                pic.encode_pic_flags = NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
            }
            self.frame_idx = self.frame_idx.wrapping_add(1);
            self.call(
                (self.api.encode)(self.enc, &mut *pic),
                "nvEncEncodePicture",
            )?;

            let mut lock: NV_ENC_LOCK_BITSTREAM = zeroed();
            lock.version = NV_ENC_LOCK_BITSTREAM_VER;
            lock.output_bitstream = self.bitstream;
            self.call(
                (self.api.lock)(self.enc, &mut lock),
                "nvEncLockBitstream",
            )?;
            let data = std::slice::from_raw_parts(
                lock.bitstream_buffer_ptr as *const u8,
                lock.bitstream_size_in_bytes as usize,
            )
            .to_vec();
            self.call(
                (self.api.unlock)(self.enc, self.bitstream),
                "nvEncUnlockBitstream",
            )?;
            Ok(data)
        }
    }

    fn call(&self, status: NVENCSTATUS, what: &str) -> Result<()> {
        if status == NV_ENC_SUCCESS {
            return Ok(());
        }
        let detail = unsafe {
            let p = (self.api.last_error)(self.enc);
            if p.is_null() {
                String::new()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        Err(anyhow!("{what} 失败,NVENCSTATUS = {status}: {detail}"))
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        if self.enc.is_null() {
            return;
        }
        unsafe {
            if !self.registered.is_null() {
                (self.api.unregister)(self.enc, self.registered);
            }
            if !self.bitstream.is_null() {
                (self.api.destroy_bitstream)(self.enc, self.bitstream);
            }
            (self.api.destroy)(self.enc);
        }
    }
}
