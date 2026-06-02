//! HID PMU temperature sensors for Apple Silicon (primary path).
//!
//! Enumerates `IOHIDEventSystem` services matching `PrimaryUsagePage = 0xff00`
//! and `PrimaryUsage = 5` (temperature), then reads
//! `kIOHIDEventTypeTemperature` events. Uses the private IOHIDEventSystem
//! API — these symbols are not in the public IOKit headers.

use anyhow::{Result, anyhow};
use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation_sys::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
use core_foundation_sys::base::{CFAllocatorRef, CFRelease, CFTypeRef, kCFAllocatorDefault};
use core_foundation_sys::dictionary::CFDictionaryRef;
use core_foundation_sys::string::CFStringRef;
use std::ffi::c_void;

#[derive(Debug, Clone)]
pub struct SensorReading {
    pub name: String,
    pub celsius: f64,
}

#[repr(C)]
pub struct __IOHIDEventSystemClient(c_void);
pub type IOHIDEventSystemClientRef = *mut __IOHIDEventSystemClient;

#[repr(C)]
pub struct __IOHIDServiceClient(c_void);
pub type IOHIDServiceClientRef = *mut __IOHIDServiceClient;

#[repr(C)]
pub struct __IOHIDEvent(c_void);
pub type IOHIDEventRef = *mut __IOHIDEvent;

const K_IOHID_EVENT_TYPE_TEMPERATURE: i64 = 15;
const IOHID_TEMPERATURE_FIELD: i64 = K_IOHID_EVENT_TYPE_TEMPERATURE << 16;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOHIDEventSystemClientCreate(allocator: CFAllocatorRef) -> IOHIDEventSystemClientRef;
    fn IOHIDEventSystemClientSetMatching(
        client: IOHIDEventSystemClientRef,
        matching: CFDictionaryRef,
    ) -> i32;
    fn IOHIDEventSystemClientCopyServices(client: IOHIDEventSystemClientRef) -> CFArrayRef;
    fn IOHIDServiceClientCopyProperty(
        service: IOHIDServiceClientRef,
        key: CFStringRef,
    ) -> CFTypeRef;
    fn IOHIDServiceClientCopyEvent(
        service: IOHIDServiceClientRef,
        event_type: i64,
        options: i64,
        flags: i64,
    ) -> IOHIDEventRef;
    fn IOHIDEventGetFloatValue(event: IOHIDEventRef, field: i64) -> f64;
}

pub fn read_all() -> Result<Vec<SensorReading>> {
    unsafe { read_all_inner() }
}

unsafe fn read_all_inner() -> Result<Vec<SensorReading>> {
    let client = unsafe { IOHIDEventSystemClientCreate(kCFAllocatorDefault) };
    if client.is_null() {
        return Err(anyhow!("IOHIDEventSystemClientCreate returned null"));
    }

    let matching = build_matching_dict(0xff00, 5);
    // Return code semantics for this private API are not stable across OS versions;
    // prior art (fermion-star/apple_sensors, exelban/stats) calls it and ignores the result.
    let _ = unsafe { IOHIDEventSystemClientSetMatching(client, matching.as_concrete_TypeRef()) };

    let services_ref = unsafe { IOHIDEventSystemClientCopyServices(client) };
    if services_ref.is_null() {
        unsafe { CFRelease(client as CFTypeRef) };
        return Ok(Vec::new());
    }

    let count = unsafe { CFArrayGetCount(services_ref) };
    let product_key = CFString::from_static_string("Product");
    let mut readings = Vec::with_capacity(count.max(0) as usize);

    for i in 0..count {
        let service =
            unsafe { CFArrayGetValueAtIndex(services_ref, i) } as IOHIDServiceClientRef;
        if service.is_null() {
            continue;
        }

        let name_ref =
            unsafe { IOHIDServiceClientCopyProperty(service, product_key.as_concrete_TypeRef()) };
        let name = if name_ref.is_null() {
            format!("<sensor #{i}>")
        } else {
            let s = unsafe { CFString::wrap_under_create_rule(name_ref as CFStringRef) };
            s.to_string()
        };

        let event = unsafe {
            IOHIDServiceClientCopyEvent(service, K_IOHID_EVENT_TYPE_TEMPERATURE, 0, 0)
        };
        if event.is_null() {
            continue;
        }
        let celsius = unsafe { IOHIDEventGetFloatValue(event, IOHID_TEMPERATURE_FIELD) };
        unsafe { CFRelease(event as CFTypeRef) };

        readings.push(SensorReading { name, celsius });
    }

    unsafe { CFRelease(services_ref as CFTypeRef) };
    unsafe { CFRelease(client as CFTypeRef) };

    Ok(readings)
}

fn build_matching_dict(primary_usage_page: i32, primary_usage: i32) -> CFDictionary<CFType, CFType> {
    let page_key = CFString::from_static_string("PrimaryUsagePage").as_CFType();
    let usage_key = CFString::from_static_string("PrimaryUsage").as_CFType();
    let page_val = CFNumber::from(primary_usage_page).as_CFType();
    let usage_val = CFNumber::from(primary_usage).as_CFType();
    CFDictionary::from_CFType_pairs(&[(page_key, page_val), (usage_key, usage_val)])
}
