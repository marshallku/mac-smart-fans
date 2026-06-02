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
use serde::{Deserialize, Serialize};
use std::ffi::{CString, c_char, c_void};
use std::mem::{MaybeUninit, size_of};

#[derive(Debug, Clone)]
pub struct FanReading {
    pub index: u8,
    pub actual_rpm: f64,
    pub min_rpm: f64,
    pub max_rpm: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeyInfo {
    pub data_type: String,
    pub data_size: u32,
    pub data_attributes: u8,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ModeKeyCasing {
    Upper,
    Lower,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FanProbe {
    pub index: u8,
    pub mode_key_casing: Option<ModeKeyCasing>,
    pub md: Option<KeyInfo>,
    pub tg: Option<KeyInfo>,
    pub mn: Option<KeyInfo>,
    pub mx: Option<KeyInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Probe {
    pub fan_count: u8,
    pub ftst: Option<KeyInfo>,
    pub fans: Vec<FanProbe>,
}

impl Probe {
    pub fn not_controllable_reason(&self) -> Option<String> {
        if self.fan_count == 0 {
            return Some("fan_count == 0".to_string());
        }
        for fan in &self.fans {
            if fan.md.is_none() {
                return Some(format!("F{i}Md and F{i}md both missing", i = fan.index));
            }
            if fan.tg.is_none() {
                return Some(format!("F{}Tg missing", fan.index));
            }
        }
        None
    }

    pub fn controllable(&self) -> bool {
        self.not_controllable_reason().is_none()
    }
}

type KernReturn = i32;
type MachPort = u32;
type IoObject = MachPort;
type IoService = IoObject;
type IoConnect = IoObject;

const KERN_SUCCESS: KernReturn = 0;
const KERNEL_INDEX_SMC: u32 = 2;
const SMC_CMD_READ_BYTES: u8 = 5;
const SMC_CMD_WRITE_BYTES: u8 = 6;
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

    fn read_key_info(&self, key: u32) -> Option<KeyInfo> {
        let out = self
            .call(&SmcKeyData {
                key,
                data8: SMC_CMD_READ_KEYINFO,
                ..Default::default()
            })
            .ok()?;
        Some(key_info_from_raw(&out.key_info))
    }

    fn write_key(&self, key: u32, data_type: u32, data: &[u8]) -> Result<()> {
        if data.len() > 32 {
            return Err(anyhow!("smc write payload > 32 bytes"));
        }
        let mut bytes = [0u8; 32];
        bytes[..data.len()].copy_from_slice(data);
        let input = SmcKeyData {
            key,
            key_info: SmcKeyDataKeyInfo {
                data_size: data.len() as u32,
                data_type,
                data_attributes: 0,
            },
            data8: SMC_CMD_WRITE_BYTES,
            bytes,
            ..Default::default()
        };
        let _ = self.call(&input)?;
        Ok(())
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

fn fourcc_to_string(t: u32) -> String {
    String::from_utf8_lossy(&t.to_be_bytes()).into_owned()
}

fn key_info_from_raw(raw: &SmcKeyDataKeyInfo) -> KeyInfo {
    KeyInfo {
        data_type: fourcc_to_string(raw.data_type),
        data_size: raw.data_size,
        data_attributes: raw.data_attributes,
    }
}

pub fn select_mode_casing(
    upper: Option<KeyInfo>,
    lower: Option<KeyInfo>,
) -> (Option<ModeKeyCasing>, Option<KeyInfo>) {
    match (upper, lower) {
        (Some(k), _) => (Some(ModeKeyCasing::Upper), Some(k)),
        (None, Some(k)) => (Some(ModeKeyCasing::Lower), Some(k)),
        (None, None) => (None, None),
    }
}

const KEY_FTST: u32 = fourcc(b"Ftst");

pub fn probe() -> Result<Probe> {
    let smc = SmcConnection::open()?;
    let (count_info, count_bytes) = smc.read_key(KEY_FNUM)?;
    let count = decode_numeric(&count_info, &count_bytes).unwrap_or(0.0) as u8;

    let ftst = smc.read_key_info(KEY_FTST);

    let mut fans = Vec::with_capacity(count as usize);
    for i in 0..count {
        let md_upper = smc.read_key_info(fan_key(i, b"Md"));
        let md_lower = smc.read_key_info(fan_key(i, b"md"));
        let (mode_key_casing, md) = select_mode_casing(md_upper, md_lower);

        fans.push(FanProbe {
            index: i,
            mode_key_casing,
            md,
            tg: smc.read_key_info(fan_key(i, b"Tg")),
            mn: smc.read_key_info(fan_key(i, b"Mn")),
            mx: smc.read_key_info(fan_key(i, b"Mx")),
        });
    }

    Ok(Probe {
        fan_count: count,
        ftst,
        fans,
    })
}

fn fourcc_from_str(s: &str) -> Result<u32> {
    let b = s.as_bytes();
    if b.len() != 4 {
        return Err(anyhow!("fourcc string must be 4 bytes: {s:?}"));
    }
    Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn encode_one(data_type: &str, data_size: u32) -> Result<Vec<u8>> {
    match (data_type, data_size) {
        ("ui8 ", 1) => Ok(vec![1]),
        ("flt ", 4) => Ok(1.0_f32.to_le_bytes().to_vec()),
        _ => Err(anyhow!(
            "unsupported mode key type/size: {data_type:?}/{data_size}"
        )),
    }
}

fn encode_zero(data_type: &str, data_size: u32) -> Result<Vec<u8>> {
    match (data_type, data_size) {
        ("ui8 ", 1) => Ok(vec![0]),
        ("flt ", 4) => Ok(0.0_f32.to_le_bytes().to_vec()),
        _ => Err(anyhow!(
            "unsupported mode key type/size: {data_type:?}/{data_size}"
        )),
    }
}

fn encode_target(data_type: &str, data_size: u32, value: f64) -> Result<Vec<u8>> {
    if !value.is_finite() {
        return Err(anyhow!("target RPM must be finite (got {value})"));
    }
    match (data_type, data_size) {
        ("flt ", 4) => Ok((value as f32).to_le_bytes().to_vec()),
        ("fpe2", 2) => {
            let scaled = (value * 4.0).round().clamp(0.0, u16::MAX as f64) as u16;
            Ok(scaled.to_be_bytes().to_vec())
        }
        _ => Err(anyhow!(
            "unsupported target key type/size: {data_type:?}/{data_size}"
        )),
    }
}

fn compose_mode_key(fan_index: u8, casing: ModeKeyCasing) -> u32 {
    let (c1, c2) = match casing {
        ModeKeyCasing::Upper => (b'M', b'd'),
        ModeKeyCasing::Lower => (b'm', b'd'),
    };
    u32::from_be_bytes([b'F', b'0' + fan_index, c1, c2])
}

fn compose_target_key(fan_index: u8) -> u32 {
    u32::from_be_bytes([b'F', b'0' + fan_index, b'T', b'g'])
}

pub struct ManualFanSession {
    smc: SmcConnection,
    fan_index: u8,
    mode_key: u32,
    mode_type: String,
    mode_size: u32,
    armed: bool,
    restored: bool,
}

impl ManualFanSession {
    pub fn arm(fan_index: u8, casing: ModeKeyCasing, mode_info: &KeyInfo) -> Result<Self> {
        let smc = SmcConnection::open()?;
        let mode_key = compose_mode_key(fan_index, casing);
        let mode_type_fcc = fourcc_from_str(&mode_info.data_type)?;
        let payload = encode_one(&mode_info.data_type, mode_info.data_size)?;

        smc.write_key(mode_key, mode_type_fcc, &payload)?;
        std::thread::sleep(std::time::Duration::from_millis(500));

        let (rb_info, rb_bytes) = smc.read_key(mode_key)?;
        let rb = decode_numeric(&rb_info, &rb_bytes).unwrap_or(0.0);
        if rb < 0.5 {
            return Err(anyhow!(
                "direct mode write rejected (Ftst absent — no fallback)"
            ));
        }

        Ok(Self {
            smc,
            fan_index,
            mode_key,
            mode_type: mode_info.data_type.clone(),
            mode_size: mode_info.data_size,
            armed: true,
            restored: false,
        })
    }

    pub fn write_target(&self, target_info: &KeyInfo, target_rpm: f64) -> Result<()> {
        let key = compose_target_key(self.fan_index);
        let fcc = fourcc_from_str(&target_info.data_type)?;
        let payload = encode_target(&target_info.data_type, target_info.data_size, target_rpm)?;
        self.smc.write_key(key, fcc, &payload)
    }

    pub fn restore(&mut self) -> Result<bool> {
        if !self.armed || self.restored {
            return Ok(true);
        }
        let fcc = fourcc_from_str(&self.mode_type)?;
        let payload = encode_zero(&self.mode_type, self.mode_size)?;

        for _ in 0..3 {
            self.smc.write_key(self.mode_key, fcc, &payload)?;
            std::thread::sleep(std::time::Duration::from_millis(500));
            let (rb_info, rb_bytes) = self.smc.read_key(self.mode_key)?;
            let rb = decode_numeric(&rb_info, &rb_bytes).unwrap_or(1.0);
            if rb < 0.5 {
                self.restored = true;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Read the live F{N}Md value. Errors propagate so a stale SMC handle (e.g. after a
    /// sleep/wake transition) surfaces to the caller instead of being silently treated as
    /// "no drift".
    pub fn read_mode(&self) -> Result<f64> {
        let (info, bytes) = self.smc.read_key(self.mode_key)?;
        decode_numeric(&info, &bytes).ok_or_else(|| {
            anyhow!(
                "could not decode F{}Md ({}/{})",
                self.fan_index,
                self.mode_type,
                self.mode_size
            )
        })
    }

    /// Re-arm manual mode after a drift (e.g. sleep/wake firmware reset, thermalmonitord
    /// override). Writes Md=1, sleeps 500ms, verifies readback. No-op if session is
    /// already restored or was never armed.
    pub fn re_arm(&mut self) -> Result<bool> {
        if !Self::needs_re_arm(self.armed, self.restored) {
            return Ok(false);
        }
        let fcc = fourcc_from_str(&self.mode_type)?;
        let payload = encode_one(&self.mode_type, self.mode_size)?;
        self.smc.write_key(self.mode_key, fcc, &payload)?;
        std::thread::sleep(std::time::Duration::from_millis(500));
        let (rb_info, rb_bytes) = self.smc.read_key(self.mode_key)?;
        let readback = decode_numeric(&rb_info, &rb_bytes);
        Ok(matches!(
            Self::re_arm_decision(self.armed, self.restored, readback),
            ReArmDecision::Success
        ))
    }

    /// Pure state check: a re-arm SMC write is needed only when the session is currently
    /// armed and has not been restored.
    pub fn needs_re_arm(armed: bool, restored: bool) -> bool {
        armed && !restored
    }

    /// Pure decision function for the re_arm state machine. Lets unit tests exercise
    /// all three branches without a real SMC connection.
    pub fn re_arm_decision(
        armed: bool,
        restored: bool,
        readback: Option<f64>,
    ) -> ReArmDecision {
        if !Self::needs_re_arm(armed, restored) {
            return ReArmDecision::Skip;
        }
        match readback {
            Some(rb) if rb >= 0.5 => ReArmDecision::Success,
            _ => ReArmDecision::Failed,
        }
    }

    pub fn fan_index(&self) -> u8 {
        self.fan_index
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReArmDecision {
    Skip,
    Success,
    Failed,
}

impl Drop for ManualFanSession {
    fn drop(&mut self) {
        if self.armed && !self.restored {
            let _ = self.restore();
        }
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

    #[test]
    fn fourcc_to_string_pads_with_space() {
        assert_eq!(fourcc_to_string(fourcc(b"flt ")), "flt ");
        assert_eq!(fourcc_to_string(fourcc(b"ui8 ")), "ui8 ");
        assert_eq!(fourcc_to_string(fourcc(b"F0Md")), "F0Md");
    }

    fn key(t: &str, sz: u32) -> KeyInfo {
        KeyInfo {
            data_type: t.to_string(),
            data_size: sz,
            data_attributes: 0,
        }
    }

    #[test]
    fn probe_controllable_when_all_keys_present() {
        let p = Probe {
            fan_count: 2,
            ftst: Some(key("ui8 ", 1)),
            fans: vec![
                FanProbe {
                    index: 0,
                    mode_key_casing: Some(ModeKeyCasing::Upper),
                    md: Some(key("ui8 ", 1)),
                    tg: Some(key("flt ", 4)),
                    mn: Some(key("flt ", 4)),
                    mx: Some(key("flt ", 4)),
                },
                FanProbe {
                    index: 1,
                    mode_key_casing: Some(ModeKeyCasing::Upper),
                    md: Some(key("ui8 ", 1)),
                    tg: Some(key("flt ", 4)),
                    mn: Some(key("flt ", 4)),
                    mx: Some(key("flt ", 4)),
                },
            ],
        };
        assert!(p.controllable());
        assert_eq!(p.not_controllable_reason(), None);
    }

    #[test]
    fn probe_controllable_when_ftst_absent_but_direct_keys_present() {
        // Ftst is absent on M1 Max Darwin 25.3.0; direct-mode unlock is the intended path,
        // so Ftst missing must NOT mark the host as non-controllable.
        let p = Probe {
            fan_count: 1,
            ftst: None,
            fans: vec![FanProbe {
                index: 0,
                mode_key_casing: Some(ModeKeyCasing::Upper),
                md: Some(key("ui8 ", 1)),
                tg: Some(key("flt ", 4)),
                mn: None,
                mx: None,
            }],
        };
        assert!(p.controllable());
        assert_eq!(p.not_controllable_reason(), None);
    }

    #[test]
    fn probe_not_controllable_when_fan_count_zero() {
        let p = Probe {
            fan_count: 0,
            ftst: Some(key("ui8 ", 1)),
            fans: vec![],
        };
        assert!(!p.controllable());
        assert_eq!(p.not_controllable_reason().as_deref(), Some("fan_count == 0"));
    }

    #[test]
    fn key_info_from_raw_preserves_all_fields() {
        let raw = SmcKeyDataKeyInfo {
            data_type: fourcc(b"flt "),
            data_size: 4,
            data_attributes: 0xd4,
        };
        let info = key_info_from_raw(&raw);
        assert_eq!(info.data_type, "flt ");
        assert_eq!(info.data_size, 4);
        assert_eq!(info.data_attributes, 0xd4);
    }

    #[test]
    fn select_mode_casing_prefers_upper_when_both_present() {
        let u = key("ui8 ", 1);
        let l = KeyInfo {
            data_type: "ui8 ".to_string(),
            data_size: 1,
            data_attributes: 0,
        };
        let (casing, info) = select_mode_casing(Some(u.clone()), Some(l));
        assert_eq!(casing, Some(ModeKeyCasing::Upper));
        assert_eq!(info, Some(u));
    }

    #[test]
    fn select_mode_casing_falls_back_to_lower() {
        let l = key("ui8 ", 1);
        let (casing, info) = select_mode_casing(None, Some(l.clone()));
        assert_eq!(casing, Some(ModeKeyCasing::Lower));
        assert_eq!(info, Some(l));
    }

    #[test]
    fn select_mode_casing_none_when_both_absent() {
        let (casing, info) = select_mode_casing(None, None);
        assert_eq!(casing, None);
        assert_eq!(info, None);
    }

    #[test]
    fn compose_mode_key_upper_and_lower() {
        assert_eq!(compose_mode_key(0, ModeKeyCasing::Upper), fourcc(b"F0Md"));
        assert_eq!(compose_mode_key(1, ModeKeyCasing::Upper), fourcc(b"F1Md"));
        assert_eq!(compose_mode_key(0, ModeKeyCasing::Lower), fourcc(b"F0md"));
    }

    #[test]
    fn compose_target_key_per_fan() {
        assert_eq!(compose_target_key(0), fourcc(b"F0Tg"));
        assert_eq!(compose_target_key(3), fourcc(b"F3Tg"));
    }

    #[test]
    fn encode_one_supports_ui8_and_flt() {
        assert_eq!(encode_one("ui8 ", 1).unwrap(), vec![1]);
        assert_eq!(encode_one("flt ", 4).unwrap(), 1.0_f32.to_le_bytes().to_vec());
        assert!(encode_one("ui16", 2).is_err());
    }

    #[test]
    fn encode_zero_supports_ui8_and_flt() {
        assert_eq!(encode_zero("ui8 ", 1).unwrap(), vec![0]);
        assert_eq!(encode_zero("flt ", 4).unwrap(), 0.0_f32.to_le_bytes().to_vec());
    }

    #[test]
    fn encode_target_flt_is_little_endian_f32() {
        let bytes = encode_target("flt ", 4, 1500.0).unwrap();
        assert_eq!(bytes, 1500.0_f32.to_le_bytes().to_vec());
    }

    #[test]
    fn encode_target_fpe2_is_big_endian_fixed() {
        // 100.5 → 100.5 * 4 = 402 = 0x0192
        let bytes = encode_target("fpe2", 2, 100.5).unwrap();
        assert_eq!(bytes, vec![0x01, 0x92]);
    }

    #[test]
    fn encode_target_rejects_nan_and_inf() {
        assert!(encode_target("flt ", 4, f64::NAN).is_err());
        assert!(encode_target("flt ", 4, f64::INFINITY).is_err());
        assert!(encode_target("flt ", 4, f64::NEG_INFINITY).is_err());
    }

    #[test]
    fn fourcc_from_str_round_trip() {
        let v = fourcc_from_str("F0Md").unwrap();
        assert_eq!(v, fourcc(b"F0Md"));
        assert!(fourcc_from_str("F0M").is_err());
    }

    #[test]
    fn needs_re_arm_only_when_armed_and_not_restored() {
        assert!(ManualFanSession::needs_re_arm(true, false));
        assert!(!ManualFanSession::needs_re_arm(false, false));
        assert!(!ManualFanSession::needs_re_arm(true, true));
        assert!(!ManualFanSession::needs_re_arm(false, true));
    }

    #[test]
    fn re_arm_decision_skips_when_session_inactive() {
        assert_eq!(
            ManualFanSession::re_arm_decision(false, false, Some(0.0)),
            ReArmDecision::Skip
        );
        assert_eq!(
            ManualFanSession::re_arm_decision(true, true, Some(1.0)),
            ReArmDecision::Skip
        );
        assert_eq!(
            ManualFanSession::re_arm_decision(false, true, None),
            ReArmDecision::Skip
        );
    }

    #[test]
    fn re_arm_decision_success_when_readback_at_or_above_half() {
        assert_eq!(
            ManualFanSession::re_arm_decision(true, false, Some(1.0)),
            ReArmDecision::Success
        );
        assert_eq!(
            ManualFanSession::re_arm_decision(true, false, Some(0.5)),
            ReArmDecision::Success
        );
    }

    #[test]
    fn re_arm_decision_failed_when_readback_low_or_none() {
        assert_eq!(
            ManualFanSession::re_arm_decision(true, false, Some(0.0)),
            ReArmDecision::Failed
        );
        assert_eq!(
            ManualFanSession::re_arm_decision(true, false, Some(0.49)),
            ReArmDecision::Failed
        );
        assert_eq!(
            ManualFanSession::re_arm_decision(true, false, None),
            ReArmDecision::Failed
        );
    }

    #[test]
    fn probe_not_controllable_when_md_missing() {
        let p = Probe {
            fan_count: 1,
            ftst: Some(key("ui8 ", 1)),
            fans: vec![FanProbe {
                index: 0,
                mode_key_casing: None,
                md: None,
                tg: Some(key("flt ", 4)),
                mn: None,
                mx: None,
            }],
        };
        assert!(!p.controllable());
        assert!(
            p.not_controllable_reason()
                .as_deref()
                .unwrap()
                .contains("F0Md")
        );
    }
}
