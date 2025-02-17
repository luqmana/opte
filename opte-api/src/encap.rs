// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Copyright 2022 Oxide Computer Company

use core::fmt;
use core::fmt::Debug;
use core::fmt::Display;
use core::str::FromStr;

use serde::Deserialize;
use serde::Serialize;

cfg_if! {
    if #[cfg(all(not(feature = "std"), not(test)))] {
        use alloc::string::{String, ToString};
    } else {
        use std::string::{String, ToString};
    }
}

/// A Geneve Virtual Network Identifier (VNI).
#[derive(
    Clone, Copy, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct Vni {
    // A VNI is 24-bit. By storing it this way we don't have to check
    // the value on the opte-core side to know if it's a valid VNI, we
    // just decode the bytes.
    //
    // The bytes are in network order.
    inner: [u8; 3],
}

impl Default for Vni {
    fn default() -> Self {
        Vni::new(0u32).unwrap()
    }
}

impl From<Vni> for u32 {
    fn from(vni: Vni) -> u32 {
        let bytes = vni.inner;
        u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]])
    }
}

impl FromStr for Vni {
    type Err = String;

    fn from_str(val: &str) -> Result<Self, Self::Err> {
        let n = val.parse::<u32>().map_err(|e| e.to_string())?;
        Self::new(n)
    }
}

impl Display for Vni {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", u32::from(*self))
    }
}

// There's no reason to view the VNI as its raw array, so just present
// it in a human-friendly manner.
impl Debug for Vni {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Vni {{ inner: {} }}", self)
    }
}

const VNI_MAX: u32 = 0x00_FF_FF_FF;

impl Vni {
    pub fn as_u32(&self) -> u32 {
        u32::from_be_bytes([0, self.inner[0], self.inner[1], self.inner[2]])
    }

    /// Return the bytes that represent this VNI. The bytes are in
    /// network order.
    pub fn bytes(&self) -> [u8; 3] {
        return self.inner;
    }

    /// Attempt to create a new VNI from any value which can be
    /// converted to a `u32`.
    ///
    /// # Errors
    ///
    /// Returns an error when the value exceeds the 24-bit maximum.
    pub fn new<N: Into<u32>>(val: N) -> Result<Vni, String> {
        let val = val.into();
        if val > VNI_MAX {
            return Err(format!("VNI value exceeds maximum: {}", val));
        }

        let be_bytes = val.to_be_bytes();
        Ok(Vni { inner: [be_bytes[1], be_bytes[2], be_bytes[3]] })
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn good_vni() {
        assert!(Vni::new(0u32).is_ok());
        assert!(Vni::new(11u8).is_ok());
        assert!(Vni::new(VNI_MAX).is_ok());
    }

    #[test]
    fn bad_vni() {
        assert!(Vni::new(2u32.pow(24)).is_err());
        assert!(Vni::new(2u32.pow(30)).is_err());
    }

    #[test]
    fn vni_round_trip() {
        let vni = Vni::new(7777u32).unwrap();
        assert_eq!([0x00, 0x1E, 0x61], vni.inner);
        assert_eq!(7777, u32::from(vni));
    }
}
