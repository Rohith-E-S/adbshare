//! The curated debloat list: what each package is, and how risky removing it
//! is.
//!
//! The data is a snapshot of the Universal Android Debloater Next Generation
//! project's package list (GPL-3.0; see `assets/debloat/NOTICE`). It is
//! bundled rather than fetched, for two reasons: adbshare is a local-first
//! tool that does not phone home, and the list is a static dataset that
//! nothing in the app needs to change. A newer snapshot can be dropped in by
//! `make debloat-list`.
//!
//! Two things live here beyond the raw data:
//!
//! * A **vendor** for each package, worked out from its name. The upstream
//!   list has one flat "oem" bucket; a phone's bloat is really "whatever
//!   this brand preinstalled", and the package name says which brand.
//! * The **risk tier**, which is upstream's own assessment and is what the
//!   GUI sorts and filters on.

use std::collections::HashMap;
use std::io::Read;
use std::sync::LazyLock;

use serde::Deserialize;

use crate::protocol::PackageDto;

/// The bundled list, compressed.
const LIST_GZ: &[u8] = include_bytes!("../../assets/debloat/uad_lists.json.gz");

/// How risky removing a package is, in upstream's own words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum Removal {
    /// Safe to remove.
    #[default]
    Recommended,
    /// Breaks obscure or minor parts of functionality.
    Advanced,
    /// Breaks widespread functionality, but should not boot-loop the device.
    Expert,
    /// Can break vital parts of the operating system.
    Unsafe,
}

impl Removal {
    /// The name shown on the pill.
    pub fn label(self) -> &'static str {
        match self {
            Removal::Recommended => "Recommended",
            Removal::Advanced => "Advanced",
            Removal::Expert => "Expert",
            Removal::Unsafe => "Unsafe",
        }
    }

    /// One sentence about what this tier means.
    pub fn explanation(self) -> &'static str {
        match self {
            Removal::Recommended => "Safe to remove. You may still want to keep it.",
            Removal::Advanced => {
                "Breaks minor or obscure parts of the phone, such as an overlay or a \
                 default app you would replace anyway."
            }
            Removal::Expert => {
                "Breaks functionality you probably use, but should not stop the phone \
                 from booting."
            }
            Removal::Unsafe => {
                "Can break the phone in ways that need a factory reset to fix. Some of \
                 these are illegal to remove in some countries."
            }
        }
    }

    /// True when the action needs a second, deliberate confirmation.
    pub fn needs_confirmation(self) -> bool {
        matches!(self, Removal::Expert | Removal::Unsafe)
    }
}

/// Which curated list a package belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum ListKind {
    #[default]
    Aosp,
    Carrier,
    Google,
    Misc,
    Oem,
    Pending,
}

/// Who put a package on the phone.
///
/// The upstream list has one flat bucket for every manufacturer's software.
/// The package name says which manufacturer it was, so the vendor is worked
/// out from the name here. It is what makes the list read as "what this
/// phone's brand preinstalled" rather than "four thousand packages".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Vendor {
    Aosp,
    Google,
    Samsung,
    Xiaomi,
    Huawei,
    Motorola,
    Lenovo,
    Oppo,
    Vivo,
    Transsion,
    Asus,
    Sony,
    Lg,
    Zte,
    Tcl,
    Nothing,
    Meizu,
    Nubia,
    Carrier,
    Preinstalled,
    Chipset,
    Other,
}

impl Vendor {
    pub fn label(self) -> &'static str {
        match self {
            Vendor::Aosp => "AOSP",
            Vendor::Google => "Google",
            Vendor::Samsung => "Samsung",
            Vendor::Xiaomi => "Xiaomi",
            Vendor::Huawei => "Huawei",
            Vendor::Motorola => "Motorola",
            Vendor::Lenovo => "Lenovo",
            Vendor::Oppo => "Oppo",
            Vendor::Vivo => "Vivo",
            Vendor::Transsion => "Transsion",
            Vendor::Asus => "Asus",
            Vendor::Sony => "Sony",
            Vendor::Lg => "LG",
            Vendor::Zte => "ZTE",
            Vendor::Tcl => "TCL",
            Vendor::Nothing => "Nothing",
            Vendor::Meizu => "Meizu",
            Vendor::Nubia => "Nubia",
            Vendor::Carrier => "Carrier",
            Vendor::Preinstalled => "Preinstalled",
            Vendor::Chipset => "Chipset",
            Vendor::Other => "Other",
        }
    }
}

/// The prefix rules, in the order they are tried.
///
/// More specific prefixes come first: `com.samsung` has to be reached before
/// `com.s` would swallow something else, and `com.mi` before `com.m`.
const VENDOR_RULES: &[(&str, Vendor)] = &[
    ("com.sec.android", Vendor::Samsung),
    ("com.samsung", Vendor::Samsung),
    ("com.osp.app", Vendor::Samsung),
    ("com.miui", Vendor::Xiaomi),
    ("com.xiaomi", Vendor::Xiaomi),
    ("com.mi.android", Vendor::Xiaomi),
    ("com.mi.global", Vendor::Xiaomi),
    ("com.mi.", Vendor::Xiaomi),
    ("android.miui", Vendor::Xiaomi),
    ("com.huawei", Vendor::Huawei),
    ("com.hihonor", Vendor::Huawei),
    ("com.honor", Vendor::Huawei),
    ("com.motorola", Vendor::Motorola),
    ("com.lenovo", Vendor::Lenovo),
    ("com.zui", Vendor::Lenovo),
    ("com.oneplus", Vendor::Oppo),
    ("net.oneplus", Vendor::Oppo),
    ("cn.oneplus", Vendor::Oppo),
    ("com.oppo", Vendor::Oppo),
    ("com.realme", Vendor::Oppo),
    ("com.coloros", Vendor::Oppo),
    ("com.heytap", Vendor::Oppo),
    ("com.oplus", Vendor::Oppo),
    ("com.vivo", Vendor::Vivo),
    ("com.iqoo", Vendor::Vivo),
    ("com.bbk", Vendor::Vivo),
    ("com.nearme", Vendor::Vivo),
    ("com.transsion", Vendor::Transsion),
    ("com.itel", Vendor::Transsion),
    ("com.infinix", Vendor::Transsion),
    ("com.asus", Vendor::Asus),
    ("com.ape", Vendor::Asus),
    ("com.sonymobile", Vendor::Sony),
    ("com.sonyericsson", Vendor::Sony),
    ("com.sony", Vendor::Sony),
    ("com.lge", Vendor::Lg),
    ("com.zte", Vendor::Zte),
    ("com.tcl", Vendor::Tcl),
    ("com.tct", Vendor::Tcl),
    ("com.nothing", Vendor::Nothing),
    ("com.meizu", Vendor::Meizu),
    ("com.flyme", Vendor::Meizu),
    ("cn.nubia", Vendor::Nubia),
    ("com.nubia", Vendor::Nubia),
    ("com.qualcomm", Vendor::Chipset),
    ("com.qti", Vendor::Chipset),
    ("com.qcom", Vendor::Chipset),
    ("com.mediatek", Vendor::Chipset),
    ("com.mtk", Vendor::Chipset),
    ("com.sprd", Vendor::Chipset),
    ("com.spreadtrum", Vendor::Chipset),
    ("com.unisoc", Vendor::Chipset),
    ("com.google", Vendor::Google),
    ("com.android", Vendor::Aosp),
    ("android.", Vendor::Aosp),
    ("org.lineageos", Vendor::Aosp),
    ("com.evenwell", Vendor::Preinstalled),
    ("com.monotype", Vendor::Preinstalled),
    ("com.baidu", Vendor::Preinstalled),
    ("com.goodix", Vendor::Preinstalled),
    ("com.fingerprints", Vendor::Preinstalled),
];

/// One package, as the curated list describes it.
#[derive(Debug, Clone, Deserialize)]
pub struct Entry {
    #[serde(default)]
    pub list: ListKind,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub needed_by: Vec<String>,
    #[serde(default)]
    pub removal: Removal,
}

/// The whole curated list.
pub struct DebloatList {
    entries: HashMap<String, Entry>,
}

/// The bundled list, read once.
///
/// Parsing five thousand entries takes long enough to be worth keeping off
/// the startup path, and the list is only needed once the debloat view is
/// opened.
static LIST: LazyLock<DebloatList> = LazyLock::new(|| {
    DebloatList::from_gzip(LIST_GZ).unwrap_or_else(|err| {
        // The list is bundled, so a failure here is a broken build rather
        // than something a user can do anything about. An empty list leaves
        // the debloat view working, just without the descriptions.
        tracing::warn!(error = %err, "the bundled debloat list could not be read");
        DebloatList {
            entries: HashMap::new(),
        }
    })
});

impl DebloatList {
    /// The bundled list.
    pub fn bundled() -> &'static DebloatList {
        &LIST
    }

    /// Read a gzipped list.
    pub fn from_gzip(data: &[u8]) -> anyhow::Result<Self> {
        let mut decoded = String::new();
        flate2::read::GzDecoder::new(data).read_to_string(&mut decoded)?;
        let entries = serde_json::from_str(&decoded)?;
        Ok(Self { entries })
    }

    /// What the list says about a package, if it says anything.
    pub fn get(&self, package: &str) -> Option<&Entry> {
        self.entries.get(package)
    }

    /// How many packages the list describes.
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Who put a package on the phone.
///
/// The curated list knows which bucket a package is in; a carrier's software
/// is a carrier's, and everything else is worked out from the package name.
pub fn vendor_of(id: &str, list: ListKind) -> Vendor {
    if list == ListKind::Carrier {
        return Vendor::Carrier;
    }
    for (prefix, vendor) in VENDOR_RULES {
        if id.starts_with(prefix) {
            return *vendor;
        }
    }
    Vendor::Other
}

/// What the list says about a phone's packages, in numbers.
///
/// The sidebar shows this, so it is worth having without building the whole
/// list: how many of the phone's packages the list has an opinion about, and
/// how many of those it calls safe to remove.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Packages on the phone.
    pub total: usize,
    /// Of those, the ones the curated list knows about.
    pub listed: usize,
    /// Of those, the ones it says are safe to remove.
    pub recommended: usize,
}

impl Counts {
    /// Count a phone's packages against the curated list.
    pub fn of(packages: &[PackageDto]) -> Counts {
        let list = DebloatList::bundled();
        let mut counts = Counts {
            total: packages.len(),
            ..Default::default()
        };
        for package in packages {
            if let Some(entry) = list.get(&package.id) {
                counts.listed += 1;
                if entry.removal == Removal::Recommended {
                    counts.recommended += 1;
                }
            }
        }
        counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundled_list_reads() {
        let list = DebloatList::bundled();
        assert!(
            list.len() > 5000,
            "the list looks truncated: {}",
            list.len()
        );
        // A few anchors across the different buckets.
        let bixby = list
            .get("com.samsung.android.bixby.service")
            .expect("bixby");
        assert_eq!(bixby.list, ListKind::Oem);
        assert_eq!(bixby.removal, Removal::Recommended);
        assert!(list.get("com.google.android.gms").is_some());
        assert!(list.get("com.android.chrome").is_some());
    }

    #[test]
    fn the_vendor_follows_the_package_name() {
        let oem = ListKind::Oem;
        assert_eq!(
            vendor_of("com.samsung.android.bixby.service", oem),
            Vendor::Samsung
        );
        assert_eq!(
            vendor_of("com.sec.android.app.shealth", oem),
            Vendor::Samsung
        );
        assert_eq!(vendor_of("com.miui.securitycenter", oem), Vendor::Xiaomi);
        assert_eq!(vendor_of("com.xiaomi.mibrain.speech", oem), Vendor::Xiaomi);
        assert_eq!(vendor_of("com.huawei.hwid", oem), Vendor::Huawei);
        assert_eq!(vendor_of("com.oneplus.battery", oem), Vendor::Oppo);
        assert_eq!(vendor_of("com.vivo.stepcount", oem), Vendor::Vivo);
        assert_eq!(
            vendor_of("com.transsion.XOSLauncher", oem),
            Vendor::Transsion
        );
        assert_eq!(vendor_of("com.lge.wificall", oem), Vendor::Lg);
        assert_eq!(vendor_of("com.motorola.omadm.usc", oem), Vendor::Motorola);
        assert_eq!(vendor_of("com.qualcomm.qti.auth", oem), Vendor::Chipset);
        assert_eq!(vendor_of("com.example.whatever", oem), Vendor::Other);
    }

    #[test]
    fn a_carrier_package_is_a_carriers() {
        assert_eq!(
            vendor_of("com.nttdocomo.android.mymagazine", ListKind::Carrier),
            Vendor::Carrier
        );
    }

    #[test]
    fn the_risk_tiers_are_ordered_by_how_bad_they_are() {
        assert!(Removal::Recommended < Removal::Advanced);
        assert!(Removal::Advanced < Removal::Expert);
        assert!(Removal::Expert < Removal::Unsafe);
        assert!(!Removal::Recommended.needs_confirmation());
        assert!(Removal::Expert.needs_confirmation());
        assert!(Removal::Unsafe.needs_confirmation());
    }
}
