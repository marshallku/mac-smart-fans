//! SMC (System Management Controller) access for Apple Silicon fan keys.
//!
//! Reads `FNum`, `F{N}Ac`, `F{N}Mn`, `F{N}Mx`. On Apple Silicon these are
//! reported as 4-byte IEEE 754 little-endian floats (`flt`); legacy `fpe2`
//! and integer types are decoded as fallback. Write path (`Ftst` unlock,
//! `F{N}Md`, `F{N}Tg`) lives in Stage 2.
//!
//! Layout of `SmcKeyData` and the `IOConnectCallStructMethod` selector index
//! follow hholtmann/smcFanControl/smc-command/smc.h.

use anyhow::{Result, anyhow};
use core_foundation_sys::dictionary::{CFDictionaryRef, CFMutableDictionaryRef};
use std::ffi::{CString, c_char, c_void};
use std::mem::{MaybeUninit, size_of};

#[derive(Debug, Clone)]
pub struct FanReading {
    pub index: u8,
    pub actual_rpm: f64,
    pub min_rpm: f64,
    pub max_rpm: f64,
}

type KernReturn = i32;
type MachPort = u32;
type IoObject = MachPort;
type IoService = IoObject;
type IoConnect = IoObject;

const KERN_SUCCESS: KernReturn = 0;
const KERNEL_INDEX_SMC: u32 = 2;
const SMC_CMD_READ_BYTES: u8 = 5;
const SMC_CMD_READ_KEYINFO: u8 = 9;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> CFMutableDictionaryRef;
    fn IOServiceGetMatchingService(main_port: MachPort, matching: CFDictionaryRef) -> IoService;
    fn IOServiceOpen(
        service: IoService,
        owning_task: MachPort,
        type_: u32,
        connect: *mut IoConnect,
    ) -> KernReturn;
    fn IOServiceClose(connect: IoConnect) -> KernReturn;
    fn IOObjectRelease(object: IoObject) -> KernReturn;
    fn IOConnectCallStructMethod(
        connection: IoConnect,
        selector: u32,
        input_struct: *const c_void,
        input_struct_cnt: usize,
        output_struct: *mut c_void,
        output_struct_cnt: *mut usize,
    ) -> KernReturn;
}

unsafe extern "C" {
    static mach_task_self_: MachPort;
}

#[repr(C)]
#[derive(Default, Copy, Clone)]
struct SmcKeyDataVers {
    major: u8,
    minor: u8,
    build: u8,
    reserved: [u8; 1],
    release: u16,
}

#[repr(C)]
#[derive(Default, Copy, Clone)]
struct SmcKeyDataPLimitData {
    version: u16,
    length: u16,
    cpu_p_limit: u32,
    gpu_p_limit: u32,
    mem_p_limit: u32,
}

#[repr(C)]
#[derive(Default, Copy, Clone)]
struct SmcKeyDataKeyInfo {
    data_size: u32,
    data_type: u32,
    data_attributes: u8,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct SmcKeyData {
    key: u32,
    vers: SmcKeyDataVers,
    p_limit_data: SmcKeyDataPLimitData,
    key_info: SmcKeyDataKeyInfo,
    result: u8,
    status: u8,
    data8: u8,
    data32: u32,
    bytes: [u8; 32],
}

impl Default for SmcKeyData {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

const KEY_FNUM: u32 = fourcc(b"FNum");

fn fan_key(index: u8, suffix: &[u8; 2]) -> u32 {
    let bytes = [b'F', b'0' + index, suffix[0], suffix[1]];
    fourcc(&bytes)
}

struct SmcConnection {
    conn: IoConnect,
}

impl SmcConnection {
    fn open() -> Result<Self> {
        unsafe {
            let name = CString::new("AppleSMC").unwrap();
            let matching = IOServiceMatching(name.as_ptr());
            if matching.is_null() {
                return Err(anyhow!("IOServiceMatching(AppleSMC) returned null"));
            }
            let service = IOServiceGetMatchingService(0, matching as CFDictionaryRef);
            if service == 0 {
                return Err(anyhow!("AppleSMC service not found"));
            }
            let mut conn: IoConnect = 0;
            let rc = IOServiceOpen(service, mach_task_self_, 0, &mut conn);
            IOObjectRelease(service);
            if rc != KERN_SUCCESS {
                return Err(anyhow!("IOServiceOpen(AppleSMC) failed: 0x{:x}", rc));
            }
            Ok(Self { conn })
        }
    }

    fn call(&self, input: &SmcKeyData) -> Result<SmcKeyData> {
        let mut output = MaybeUninit::<SmcKeyData>::zeroed();
        let mut out_size = size_of::<SmcKeyData>();
        let rc = unsafe {
            IOConnectCallStructMethod(
                self.conn,
                KERNEL_INDEX_SMC,
                input as *const SmcKeyData as *const c_void,
                size_of::<SmcKeyData>(),
                output.as_mut_ptr() as *mut c_void,
                &mut out_size,
            )
        };
        if rc != KERN_SUCCESS {
            return Err(anyhow!("IOConnectCallStructMethod failed: 0x{:x}", rc));
        }
        let out = unsafe { output.assume_init() };
        if out.result != 0 {
            return Err(anyhow!("SMC returned result=0x{:x}", out.result));
        }
        Ok(out)
    }

    fn read_key(&self, key: u32) -> Result<(SmcKeyDataKeyInfo, [u8; 32])> {
        let info_out = self.call(&SmcKeyData {
            key,
            data8: SMC_CMD_READ_KEYINFO,
            ..Default::default()
        })?;
        let info = info_out.key_info;

        let value_out = self.call(&SmcKeyData {
            key,
            key_info: info,
            data8: SMC_CMD_READ_BYTES,
            ..Default::default()
        })?;
        Ok((info, value_out.bytes))
    }
}

impl Drop for SmcConnection {
    fn drop(&mut self) {
        unsafe {
            IOServiceClose(self.conn);
        }
    }
}

fn decode_numeric(info: &SmcKeyDataKeyInfo, bytes: &[u8; 32]) -> Option<f64> {
    match (&info.data_type.to_be_bytes(), info.data_size) {
        (b"flt ", 4) => {
            let arr: [u8; 4] = bytes[..4].try_into().ok()?;
            Some(f32::from_le_bytes(arr) as f64)
        }
        (b"fpe2", 2) => {
            let n = u16::from_be_bytes([bytes[0], bytes[1]]);
            Some((n as f64) / 4.0)
        }
        (b"ui8 ", 1) => Some(bytes[0] as f64),
        (b"ui16", 2) => Some(u16::from_be_bytes([bytes[0], bytes[1]]) as f64),
        (b"ui32", 4) => Some(
            u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f64,
        ),
        _ => None,
    }
}

pub fn read_fans() -> Result<Vec<FanReading>> {
    let smc = SmcConnection::open()?;
    let (count_info, count_bytes) = smc.read_key(KEY_FNUM)?;
    let count = decode_numeric(&count_info, &count_bytes).ok_or_else(|| {
        anyhow!(
            "could not decode FNum (type={:?}, size={})",
            std::str::from_utf8(&count_info.data_type.to_be_bytes()).unwrap_or("?"),
            count_info.data_size
        )
    })? as u8;

    let mut readings = Vec::with_capacity(count as usize);
    for i in 0..count {
        let (ac_info, ac_bytes) = smc.read_key(fan_key(i, b"Ac"))?;
        let (mn_info, mn_bytes) = smc.read_key(fan_key(i, b"Mn"))?;
        let (mx_info, mx_bytes) = smc.read_key(fan_key(i, b"Mx"))?;

        readings.push(FanReading {
            index: i,
            actual_rpm: decode_numeric(&ac_info, &ac_bytes).unwrap_or(f64::NAN),
            min_rpm: decode_numeric(&mn_info, &mn_bytes).unwrap_or(f64::NAN),
            max_rpm: decode_numeric(&mx_info, &mx_bytes).unwrap_or(f64::NAN),
        });
    }
    Ok(readings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fourcc_packs_big_endian() {
        assert_eq!(fourcc(b"FNum"), 0x464E_756D);
        assert_eq!(fourcc(b"F0Ac"), 0x4630_4163);
    }

    #[test]
    fn fan_key_builds_per_index() {
        assert_eq!(fan_key(0, b"Ac"), fourcc(b"F0Ac"));
        assert_eq!(fan_key(3, b"Tg"), fourcc(b"F3Tg"));
        assert_eq!(fan_key(7, b"Md"), fourcc(b"F7Md"));
    }

    fn info_of(ty: &[u8; 4], size: u32) -> SmcKeyDataKeyInfo {
        SmcKeyDataKeyInfo {
            data_type: fourcc(ty),
            data_size: size,
            data_attributes: 0,
        }
    }

    #[test]
    fn decode_flt_little_endian_float() {
        let info = info_of(b"flt ", 4);
        let mut bytes = [0u8; 32];
        bytes[..4].copy_from_slice(&1500.0_f32.to_le_bytes());
        assert_eq!(decode_numeric(&info, &bytes), Some(1500.0));
    }

    #[test]
    fn decode_fpe2_big_endian_fixed_point() {
        let info = info_of(b"fpe2", 2);
        let mut bytes = [0u8; 32];
        bytes[0] = 0x01;
        bytes[1] = 0x92;
        assert_eq!(decode_numeric(&info, &bytes), Some(100.5));
    }

    #[test]
    fn decode_ui8_single_byte() {
        let info = info_of(b"ui8 ", 1);
        let mut bytes = [0u8; 32];
        bytes[0] = 42;
        assert_eq!(decode_numeric(&info, &bytes), Some(42.0));
    }

    #[test]
    fn decode_ui16_big_endian() {
        let info = info_of(b"ui16", 2);
        let mut bytes = [0u8; 32];
        bytes[0] = 0x12;
        bytes[1] = 0x34;
        assert_eq!(decode_numeric(&info, &bytes), Some(0x1234 as f64));
    }

    #[test]
    fn decode_rejects_unknown_type() {
        let info = info_of(b"unkn", 4);
        let bytes = [0u8; 32];
        assert!(decode_numeric(&info, &bytes).is_none());
    }
}
