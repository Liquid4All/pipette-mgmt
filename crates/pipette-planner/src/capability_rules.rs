//! Hardcoded capability-requirement rules (PIP-413): the hardware policy a
//! runtime injects into each cell's `requires` at expansion, rejecting a
//! contradiction first. Code, not config, so a policy change is a reviewed edit.

use std::collections::{HashMap, HashSet};

use thiserror::Error;

use pipette_plan_types::{
    CapabilityFlag, CapabilityFlagError, ClientId, OnnxExecutionProvider, Runtime,
};

use crate::expand::PlanWarning;
use crate::scheduler_plan::{SchedulerPlan, SchedulerVariant};

/// A capability-rule violation. The `RuleTable*` variants are typos in this
/// file, returned because the workspace bans panics; `rules_table_is_well_formed`
/// proves they cannot fire.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CapabilityRuleError {
    #[error(
        "requires must commit to exactly one of {one_of:?} (e.g. one `os:`); found none. \
         Add the flag naming the platform this variant targets"
    )]
    OneOfNoCommit { one_of: Vec<String> },

    #[error(
        "requires commits to {committed:?}, but {one_of:?} are mutually exclusive; \
         exactly one is allowed"
    )]
    OneOfMultipleCommit {
        committed: Vec<String>,
        one_of: Vec<String>,
    },

    #[error(
        "requires contains {first:?} and {second:?}, both in the mutually exclusive \
         `{namespace}:` namespace; a device has exactly one {namespace}"
    )]
    ExclusiveNamespaceConflict {
        namespace: String,
        first: String,
        second: String,
    },

    #[error(
        "requires pins {pinned:?}, but this runtime's supported `{namespace}:` family is \
         {family:?}, which excludes it; no client could satisfy both"
    )]
    PinExcludedFromFamily {
        namespace: String,
        pinned: String,
        family: Vec<String>,
    },

    #[error(
        "`excludes` names {flag:?}, which `requires` demands; no client both carries it \
         and lacks it"
    )]
    ExcludesRequiredFlag { flag: String },

    #[error(
        "the `not_all_of` group {group:?} holds only flags `requires` demands, so it \
         excludes every client `requires` admits"
    )]
    NotAllOfWithinRequires { group: Vec<String> },

    #[error(
        "`excludes` covers every member of the `any_of` group {group:?}, so no client \
         satisfies it"
    )]
    ExcludesCoversAnyOfGroup { group: Vec<String> },

    #[error(
        "the `not_all_of` group holds {first:?} and {second:?}, both in the mutually \
         exclusive `{namespace}:` namespace; no client carries the whole group, so the \
         group excludes nobody"
    )]
    VacuousExclusionGroup {
        namespace: String,
        first: String,
        second: String,
    },

    #[error(
        "`exclude_clients` names every client `clients` does, and the variant states no \
         `requires`; nothing is left to match"
    )]
    RosterWhollyExcluded,

    #[error("rules table: flag {raw:?} is not in canonical form")]
    RuleTableFlagNotCanonical {
        raw: String,
        #[source]
        source: CapabilityFlagError,
    },

    #[error("rules table: floor {floor:?} is not a member of its ordered vocabulary")]
    RuleTableUnknownFloor { floor: String },

    #[error("rules table: an `AnyOf` group has no members")]
    RuleTableEmptyGroup,
}

/// A [`CapabilityRuleError`] in one variant on one of its runtimes. The message
/// names the block an operator looks for; `variant` lets a caller place it.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("variant {variant} ({runtime} runtime)")]
pub struct VariantRuleError {
    pub variant: usize,
    pub runtime: &'static str,
    #[source]
    pub rule: CapabilityRuleError,
}

/// One cell's requirement: every flat `requires` flag plus one member of each
/// `any_of` group. Both are sorted and deduplicated, so set-equal requirements
/// compare equal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EffectiveRequirement {
    pub requires: Vec<CapabilityFlag>,
    /// Injected by the rules; a plan author never writes `any_of`.
    pub any_of: Vec<Vec<CapabilityFlag>>,
}

impl EffectiveRequirement {
    /// No capability path: the cell is eligible by its `clients` roster alone.
    const NONE: EffectiveRequirement = EffectiveRequirement {
        requires: Vec::new(),
        any_of: Vec::new(),
    };
}

/// An empty `committed` is a roster-only variant and gets no injection: the
/// server admits `clients` *or* `requires`, so an injected `os:macos` would open
/// a second path that every Mac could claim.
pub(crate) fn resolve_effective_requirement(
    committed: &[CapabilityFlag],
    runtime: &Runtime,
) -> Result<EffectiveRequirement, CapabilityRuleError> {
    if committed.is_empty() {
        return Ok(EffectiveRequirement::NONE);
    }
    resolve_from_policy(committed, &policy_for(runtime))
}

/// Exclusions are checked against the effective requirement (injected flags
/// included), which is what the server matches. A dead capability path is fatal
/// only when the `clients` roster is dead too, as at ingestion; else it warns.
pub(crate) fn validate_capability_rules(
    plan: &SchedulerPlan,
) -> Result<Vec<PlanWarning>, VariantRuleError> {
    let mut warnings = Vec::new();
    plan.variants
        .iter()
        .enumerate()
        .try_for_each(|(idx, variant)| {
            let roster_is_live = roster_is_live(&variant.clients, &variant.exclude_clients);
            // A roster every `exclude_clients` name cancels is a contradiction
            // in the variant itself, whatever the runtime resolves to.
            if !roster_is_live && !variant.clients.is_empty() {
                warnings.push(PlanWarning {
                    variant: idx,
                    message: format!(
                        "variant {idx}: `exclude_clients` denies every client `clients` names, \
                         so the roster adds nobody"
                    ),
                });
            }
            if roster_is_live && !variant.client_versions.is_empty() {
                warnings.push(PlanWarning {
                    variant: idx,
                    message: format!(
                        "variant {idx}: `client_versions` narrows only the `requires` path, so a \
                         client `clients` names runs this variant whatever version it reports"
                    ),
                });
            }
            variant.runtimes.iter().try_for_each(|runtime| {
                check_variant_runtime(idx, variant, runtime, roster_is_live, &mut warnings)
            })
        })?;
    Ok(warnings)
}

fn check_variant_runtime(
    idx: usize,
    variant: &SchedulerVariant,
    runtime: &Runtime,
    roster_is_live: bool,
    warnings: &mut Vec<PlanWarning>,
) -> Result<(), VariantRuleError> {
    let at_variant = |rule: CapabilityRuleError| VariantRuleError {
        variant: idx,
        runtime: runtime.headless_token(),
        rule,
    };
    let reject = |rule: CapabilityRuleError| Err(at_variant(rule));

    // The rules read the flags the server matches, so without the mark.
    let requires: Vec<CapabilityFlag> = variant.requires.iter().map(unmarked).collect();
    let mut effective = resolve_effective_requirement(&requires, runtime).map_err(at_variant)?;

    // A group no client carries whole excludes nobody, whatever else the
    // variant says, so this rejects on its own — as ingestion does.
    if let Some(error) = vacuous_exclusion_group(&variant.not_all_of) {
        return reject(error);
    }

    // Every `clients`-only variant lands here; the roster carries it, so this is
    // a fault only when the roster is dead too.
    if effective.requires.is_empty() {
        return if roster_is_live {
            Ok(())
        } else {
            reject(CapabilityRuleError::RosterWhollyExcluded)
        };
    }

    // The versions are the body's last `any_of` group, so they are checked as
    // one, after the injected groups.
    let versions: Vec<CapabilityFlag> = variant
        .eligibility()
        .client_versions()
        .iter()
        .map(unmarked)
        .collect();
    if !versions.is_empty() {
        effective.any_of.push(versions);
    }
    let Some(error) = dead_capability_branch(&effective, &variant.excludes, &variant.not_all_of)
    else {
        return Ok(());
    };
    if !roster_is_live {
        return reject(error);
    }
    let listed = variant.clients.len();
    let noun = if listed == 1 { "client" } else { "clients" };
    let fault = at_variant(error);
    warnings.push(PlanWarning {
        variant: idx,
        message: format!(
            "{fault}: {}, so at most the {listed} {noun} listed in `clients` can run this variant",
            fault.rule
        ),
    });
    Ok(())
}

/// Only `exclude_clients` counts: a rostered client's flags are unknowable
/// here, as they are to the server at ingestion.
fn roster_is_live(clients: &[ClientId], exclude_clients: &[ClientId]) -> bool {
    clients
        .iter()
        .any(|id| !exclude_clients.iter().any(|denied| denied == id))
}

/// Checks run in the server's order (`matching::capability_branch_reason`), so
/// faults carry the same names. No empty-group case: plan validation and
/// [`resolve_group`] rule empty groups out.
fn dead_capability_branch(
    effective: &EffectiveRequirement,
    excludes: &[CapabilityFlag],
    not_all_of: &[Vec<CapabilityFlag>],
) -> Option<CapabilityRuleError> {
    let required: HashSet<&str> = effective
        .requires
        .iter()
        .map(CapabilityFlag::as_ref)
        .collect();
    let excluded: HashSet<&str> = excludes.iter().map(CapabilityFlag::as_ref).collect();
    let owned = |group: &Vec<CapabilityFlag>| -> Vec<String> {
        group.iter().map(|f| f.as_ref().to_owned()).collect()
    };

    if let Some(flag) = effective
        .requires
        .iter()
        .find(|f| excluded.contains(f.as_ref()))
    {
        return Some(CapabilityRuleError::ExcludesRequiredFlag {
            flag: flag.as_ref().to_owned(),
        });
    }
    if let Some(group) = not_all_of
        .iter()
        .find(|group| group.iter().all(|f| required.contains(f.as_ref())))
    {
        return Some(CapabilityRuleError::NotAllOfWithinRequires {
            group: owned(group),
        });
    }
    effective
        .any_of
        .iter()
        .find(|group| group.iter().all(|f| excluded.contains(f.as_ref())))
        .map(|group| CapabilityRuleError::ExcludesCoversAnyOfGroup {
            group: owned(group),
        })
}

/// A `not_all_of` group with two *distinct* flags of one exclusive namespace:
/// no device carries it whole, so it silently excludes nobody. A repeated flag
/// is not a fault.
fn vacuous_exclusion_group(groups: &[Vec<CapabilityFlag>]) -> Option<CapabilityRuleError> {
    groups.iter().find_map(|group| {
        let mut namespaces: HashMap<&str, &str> = HashMap::new();
        group
            .iter()
            .map(CapabilityFlag::as_ref)
            .filter_map(|flag| {
                namespace_of(flag)
                    .filter(|ns| EXCLUSIVE_NAMESPACES.contains(ns))
                    .map(|ns| (ns, flag))
            })
            .find_map(|(namespace, flag)| {
                namespaces
                    .insert(namespace, flag)
                    .filter(|first| *first != flag)
                    .map(|first| CapabilityRuleError::VacuousExclusionGroup {
                        namespace: namespace.to_owned(),
                        first: first.to_owned(),
                        second: flag.to_owned(),
                    })
            })
    })
}

/// A disjunction the rules inject: satisfied by at least one member.
enum Group {
    /// The "X or newer" family. A shared vocabulary keeps floors consistent, and
    /// a new device extends every floor at once.
    AtLeast {
        list: &'static [&'static str],
        floor: &'static str,
    },
    AnyOf(&'static [&'static str]),
}

/// One runtime kind's hardware policy.
struct Policy {
    requires: &'static [&'static str],
    /// The author's own `requires` must hold exactly one of these; empty
    /// disables the check.
    one_of: &'static [&'static str],
    any_of: &'static [Group],
    /// Fired by an author-committed or unconditionally injected flag; a
    /// `when`-injected flag never triggers another `when`.
    when: &'static [When],
}

struct When {
    if_present: &'static str,
    require: &'static [&'static str],
    any_of: &'static [Group],
}

impl Policy {
    /// A runtime that demands nothing beyond what the author writes.
    const EMPTY: Policy = Policy {
        requires: &[],
        one_of: &[],
        any_of: &[],
        when: &[],
    };
}

// The rule for each runtime. The `match` has no `_` arm, so adding a
// [`Runtime`] variant will not compile until it declares a policy here.
fn policy_for(runtime: &Runtime) -> Policy {
    match runtime {
        // The author commits to iOS, iPadOS or macOS. An iPad reports `os:ipados`
        // (`DeviceProbe.detectOsName`), so it is a platform of its own.
        Runtime::AppleFoundation(_) => Policy {
            requires: &[],
            one_of: &["os:ios", "os:ipados", "os:macos"],
            any_of: &[],
            when: &[
                // iOS: iPhone 15 Pro or newer — the A17 Pro floor the
                // Foundation Models framework sets.
                When {
                    if_present: "os:ios",
                    require: &[],
                    any_of: &[Group::AtLeast {
                        list: IPHONES,
                        floor: "device:iphone15pro",
                    }],
                },
                // iPadOS: an M-series iPad or the A17 Pro iPad mini, i.e. the
                // [`IPADS`] tail from its A17 Pro entry.
                When {
                    if_present: "os:ipados",
                    require: &[],
                    any_of: &[Group::AtLeast {
                        list: IPADS,
                        floor: "device:ipadmini(a17pro)",
                    }],
                },
                // macOS: Apple silicon. The macOS 26.0 floor is deferred — see
                // the OS-VERSION FLOORS note below.
                When {
                    if_present: "os:macos",
                    require: &[],
                    any_of: &[Group::AtLeast {
                        list: APPLE_SILICON,
                        floor: "chip:applem1",
                    }],
                },
            ],
        },
        // Desktop MLX (Python/uv): Apple silicon. macOS 14 floor deferred.
        Runtime::MlxMacosPipette(_) => Policy {
            requires: &["os:macos"],
            one_of: &[],
            any_of: &[Group::AtLeast {
                list: APPLE_SILICON,
                floor: "chip:applem1",
            }],
            when: &[],
        },
        // iPhone and iPad report different `os:` flags, so the author commits to
        // one. iOS 17.2 floor deferred.
        Runtime::MlxIosPipette(_) => Policy {
            requires: &[],
            one_of: &["os:ios", "os:ipados"],
            any_of: &[],
            // A12 Bionic or newer on either platform.
            when: &[
                When {
                    if_present: "os:ios",
                    require: &[],
                    any_of: &[Group::AtLeast {
                        list: IPHONES,
                        floor: "device:iphonexs",
                    }],
                },
                When {
                    if_present: "os:ipados",
                    require: &[],
                    any_of: &[Group::AtLeast {
                        list: IPADS,
                        floor: "device:ipadmini(5thgen)",
                    }],
                },
            ],
        },
        // A GPU or an AVX-512 CPU (PIP-504). No `os:` pin: a Linux client's `os:`
        // names its distribution. On Windows `accel:gpu` is any display adapter,
        // so a plan names its hosts by tag or device flag.
        Runtime::DockerVllm(_)
        | Runtime::DockerSglang(_)
        | Runtime::UvVllm(_)
        | Runtime::UvSglang(_) => Policy {
            requires: &[],
            one_of: &[],
            any_of: &[Group::AnyOf(&["accel:gpu", "cpu_feature:avx512f"])],
            when: &[],
        },
        // No `os:` pin: a client's `os:` is the name it reports (`os:ubuntu`), so
        // the author writes that name or a tag.
        //
        // TODO(PIP-505): the real floor is `cpu_vendor:intel`.
        // TODO(PIP-501): a `device = "npu"` cell needs an Intel Core Ultra NPU and
        // strands at dispatch; the device is in runtime flags these rules never see.
        Runtime::UvOpenvino(_) => Policy::EMPTY,
        // The execution provider is the hardware target, so the policy reads it.
        // CUDA needs no `os:` pin: only Linux and Windows clients, the platforms
        // its wheels cover, report `accel:gpu`. A non-NVIDIA host it matches
        // fails before the run.
        //
        // Empty: CPU runs everywhere, DirectML awaits a catalog row naming its
        // platforms, no client serves WinML, and pipette does not own `Custom`.
        Runtime::UvOnnxGenai(u) => match u.execution_provider {
            OnnxExecutionProvider::Cuda => Policy {
                requires: &["accel:gpu"],
                one_of: &[],
                any_of: &[],
                when: &[],
            },
            OnnxExecutionProvider::Cpu
            | OnnxExecutionProvider::DirectMl
            | OnnxExecutionProvider::WinMl
            | OnnxExecutionProvider::Custom(_) => Policy::EMPTY,
        },
        // Each flavor is one OS's archive, unpinned for OpenVINO's reason; a
        // client refuses a foreign flavor at install.
        //
        // TODO(PIP-505, PIP-501): `UvOpenvino`'s deferred floors apply too.
        // TODO(PIP-507): the ubuntu24 archive needs glibc 2.38 and the redhat one
        // OpenSSL 3.4, so neither starts on Ubuntu 22.04; authors pin `os_version`.
        Runtime::Ovms(_) => Policy::EMPTY,
        // A Jetson's GPU: `accel:gpu`, which an Orin reports from nvidia-smi's
        // name alone, plus `cpu_vendor:nvidia`, which only a Tegra board reports
        // (its device-tree name; a discrete NVIDIA card sits beside another
        // vendor's CPU). The client's preflight requires the Orin SoC
        // (`nvidia,tegra234`) for a `jetson-orin-*` flavor; nothing checks the
        // JetPack version at install.
        Runtime::TrtEdgellm(_) => Policy {
            requires: &["accel:gpu", "cpu_vendor:nvidia"],
            one_of: &[],
            any_of: &[],
            when: &[],
        },
        // The author commits to iOS or iPadOS, as for MLX-iOS. Unlike MLX-iOS,
        // no device floor: onnxruntime-genai sets none and this build runs on CPU.
        Runtime::OnnxGenaiIosPipette(_) => Policy {
            requires: &[],
            one_of: &["os:ios", "os:ipados"],
            any_of: &[],
            when: &[],
        },
        // No single hardware floor across desktop platforms and flavors.
        Runtime::LlamacppCliStockTools(_) => Policy::EMPTY,
        // No device floor: llama.cpp runs on far older hardware than MLX or AFM.
        Runtime::LlamacppApkPipette(_) => Policy {
            requires: &["os:android"],
            one_of: &[],
            any_of: &[],
            when: &[],
        },
        // An iPad runs the same iOS binary and reports `os:ipados`, so the
        // author commits the platform the variant targets, as MLX-iOS does.
        Runtime::LlamacppIosPipette(_) => Policy {
            requires: &[],
            one_of: &["os:ios", "os:ipados"],
            any_of: &[],
            when: &[],
        },
    }
}

// `IPHONES` and `IPADS` mirror `ios/Pipette/Pipette/Device/Probe.swift` in
// pipette-clients, checked against the `testdata/Probe.swift` snapshot.
// When the iOS client adds a device, copy the upstream file over the snapshot
// and update these lists. `Group::AtLeast` floors index into them, so keep them
// oldest chip first.
//
// OS-VERSION FLOORS (deferred, PIP-507): a device reports its full version
// (`os_version:26.1`), so `os_version:26` would match nothing and strand every
// job, and listing every point release is unbounded. They wait on a
// server-derived major-version flag (PIP-499).

/// Every iPhone pipette tracks, **oldest chip first**.
///
/// **Warning:** confirm the fleet before flooring below the A15 phones. The
/// A12–A14 entries match only phones on a build whose `Probe.swift` has them;
/// older builds report a raw identifier (`device:iphone11,2`) and silently drop out.
const IPHONES: &[&str] = &[
    // A12 Bionic (2018) — the in-process MLX floor.
    "device:iphonexs",
    "device:iphonexsmax",
    "device:iphonexr",
    // A13 Bionic
    "device:iphone11",
    "device:iphone11pro",
    "device:iphone11promax",
    "device:iphonese(2ndgen)",
    // A14 Bionic
    "device:iphone12mini",
    "device:iphone12",
    "device:iphone12pro",
    "device:iphone12promax",
    // A15 Bionic
    "device:iphone13mini",
    "device:iphone13",
    "device:iphone13pro",
    "device:iphone13promax",
    "device:iphonese(3rdgen)",
    "device:iphone14",
    "device:iphone14plus",
    // A16 Bionic
    "device:iphone14pro",
    "device:iphone14promax",
    "device:iphone15",
    "device:iphone15plus",
    // A17 Pro — the Apple Foundation Models floor.
    "device:iphone15pro",
    "device:iphone15promax",
    // A18
    "device:iphone16",
    "device:iphone16plus",
    "device:iphone16e",
    // A18 Pro
    "device:iphone16pro",
    "device:iphone16promax",
    // A19
    "device:iphone17",
    "device:iphone17e",
    // A19 Pro
    "device:iphoneair",
    "device:iphone17pro",
    "device:iphone17promax",
    // A20 Pro
    "device:iphone18pro",
    "device:iphone18promax",
    "device:iphoneduo",
];

/// Every iPad pipette tracks, **oldest chip first**. The whole A line precedes
/// the whole M line (the 2025 A16 iPad sits below the 2021 M1 iPad Pro), so the
/// A17 Pro floor is exactly Apple Foundation Models' support matrix.
///
/// **Warning:** confirm the fleet before authoring an iPadOS variant. An iPad on
/// a build without these entries reports a raw identifier (`device:ipad14,3`).
const IPADS: &[&str] = &[
    // A12 Bionic — the in-process MLX floor.
    "device:ipadmini(5thgen)",
    "device:ipadair(3rdgen)",
    "device:ipad(8thgen)",
    // A12X Bionic
    "device:ipadpro11-inch(1stgen)",
    "device:ipadpro12.9-inch(3rdgen)",
    // A12Z Bionic
    "device:ipadpro11-inch(2ndgen)",
    "device:ipadpro12.9-inch(4thgen)",
    // A13 Bionic
    "device:ipad(9thgen)",
    // A14 Bionic
    "device:ipadair(4thgen)",
    "device:ipad(10thgen)",
    // A15 Bionic
    "device:ipadmini(6thgen)",
    // A16
    "device:ipad(a16)",
    // A17 Pro — the Apple Foundation Models floor.
    "device:ipadmini(a17pro)",
    // M1
    "device:ipadpro11-inch(3rdgen)",
    "device:ipadpro12.9-inch(5thgen)",
    "device:ipadair(5thgen)",
    // M2
    "device:ipadpro11-inch(4thgen)",
    "device:ipadpro12.9-inch(6thgen)",
    "device:ipadair11-inch(m2)",
    "device:ipadair13-inch(m2)",
    // M3
    "device:ipadair11-inch(m3)",
    "device:ipadair13-inch(m3)",
    // M4
    "device:ipadpro11-inch(m4)",
    "device:ipadpro13-inch(m4)",
    "device:ipadair11-inch(m4)",
    "device:ipadair13-inch(m4)",
    // M5
    "device:ipadpro11-inch(m5)",
    "device:ipadpro13-inch(m5)",
];

/// Apple silicon Mac SoCs by generation, then base, `Pro`, `Max`, `Ultra`. Not by
/// speed: an "M2 or newer" floor excludes the M1 Ultra on purpose.
///
/// Tiers are spelled out because the server matches the slugified brand string
/// whole ("Apple M3 Max"). The grid is not pruned to shipped SKUs: a nonexistent
/// tier matches nobody, a missing one silently strands real Macs.
const APPLE_SILICON: &[&str] = &[
    "chip:applem1",
    "chip:applem1pro",
    "chip:applem1max",
    "chip:applem1ultra",
    "chip:applem2",
    "chip:applem2pro",
    "chip:applem2max",
    "chip:applem2ultra",
    "chip:applem3",
    "chip:applem3pro",
    "chip:applem3max",
    "chip:applem3ultra",
    "chip:applem4",
    "chip:applem4pro",
    "chip:applem4max",
    "chip:applem4ultra",
    "chip:applem5",
    "chip:applem5pro",
    "chip:applem5max",
    "chip:applem5ultra",
];

/// Single-valued namespaces: a device has one of each. Mirrors the server's
/// `RESERVED_CAPABILITY_NAMESPACES`; ingestion re-checks, so drift only changes
/// how early a bad plan is caught.
const EXCLUSIVE_NAMESPACES: &[&str] = &[
    "os",
    "os_version",
    "arch",
    "os_family",
    "device",
    "chip",
    "form_factor",
    "ram_bytes",
    "gpu",
    "gpu_vram_bytes",
    "npu",
    "npu_vram_bytes",
];

/// The server's `pipette_core::matching::REQUIRED_SUFFIX`, and the namespaces
/// whose flags take it; `the_required_mark_is_the_servers` keeps both the server's.
const REQUIRED_SUFFIX: &str = ":required";
const MARKED_NAMESPACES: [&str; 2] = ["tag", "client_version"];

/// `flag` as the server matches it: without the mark ingestion takes off a tag
/// or a build.
fn unmarked(flag: &CapabilityFlag) -> CapabilityFlag {
    flag.as_ref()
        .strip_suffix(REQUIRED_SUFFIX)
        .filter(|plain| namespace_of(plain).is_some_and(|ns| MARKED_NAMESPACES.contains(&ns)))
        .and_then(|plain| CapabilityFlag::try_new(plain).ok())
        .unwrap_or_else(|| flag.clone())
}

fn namespace_of(flag: &str) -> Option<&str> {
    flag.split_once(':').map(|(namespace, _)| namespace)
}

fn resolve_from_policy(
    committed: &[CapabilityFlag],
    policy: &Policy,
) -> Result<EffectiveRequirement, CapabilityRuleError> {
    // Before any injection, so a policy never satisfies its own "choose a
    // platform" guardrail.
    check_one_of(committed, policy.one_of)?;

    let mut flat = committed.to_vec();
    flat.extend(flags(policy.requires)?);
    let mut groups = resolve_groups(policy.any_of)?;

    // Collect the triggered blocks before folding them in: `triggered` borrows
    // `flat` (via the trigger set), and the fold mutates it.
    let trigger_forms: HashSet<&str> = flat.iter().map(CapabilityFlag::as_ref).collect();
    let triggered: Vec<&When> = policy
        .when
        .iter()
        .filter(|w| trigger_forms.contains(w.if_present))
        .collect();
    triggered
        .iter()
        .try_for_each(|w| -> Result<(), CapabilityRuleError> {
            flat.extend(flags(w.require)?);
            groups.extend(resolve_groups(w.any_of)?);
            Ok(())
        })?;

    check_no_exclusive_conflict(&flat)?;
    check_pins_within_families(&flat, &groups)?;

    Ok(EffectiveRequirement {
        requires: normalized(flat),
        any_of: groups.into_iter().map(normalized).collect(),
    })
}

fn check_one_of(committed: &[CapabilityFlag], one_of: &[&str]) -> Result<(), CapabilityRuleError> {
    if one_of.is_empty() {
        return Ok(());
    }
    let chosen: Vec<String> = one_of
        .iter()
        .filter(|&&member| committed.iter().any(|f| f.as_ref() == member))
        .map(|&member| member.to_owned())
        .collect();
    let one_of_owned = || one_of.iter().map(|&m| m.to_owned()).collect::<Vec<_>>();
    match chosen.len() {
        1 => Ok(()),
        0 => Err(CapabilityRuleError::OneOfNoCommit {
            one_of: one_of_owned(),
        }),
        _ => Err(CapabilityRuleError::OneOfMultipleCommit {
            committed: chosen,
            one_of: one_of_owned(),
        }),
    }
}

/// Flat set only: an `any_of` group is many flags of one namespace by design.
fn check_no_exclusive_conflict(flat: &[CapabilityFlag]) -> Result<(), CapabilityRuleError> {
    let mut seen: HashMap<&str, &str> = HashMap::new();
    flat.iter().try_for_each(|f| {
        let flag = f.as_ref();
        let Some(namespace) = namespace_of(flag).filter(|ns| EXCLUSIVE_NAMESPACES.contains(ns))
        else {
            return Ok(());
        };
        match seen.insert(namespace, flag) {
            Some(previous) if previous != flag => {
                Err(CapabilityRuleError::ExclusiveNamespaceConflict {
                    namespace: namespace.to_owned(),
                    first: previous.to_owned(),
                    second: flag.to_owned(),
                })
            }
            _ => Ok(()),
        }
    })
}

/// Reject a flat pin its namespace's injected family excludes (`device:iphone12`
/// under AFM): a device has one `device:`, so nothing could lease the job.
fn check_pins_within_families(
    flat: &[CapabilityFlag],
    groups: &[Vec<CapabilityFlag>],
) -> Result<(), CapabilityRuleError> {
    groups.iter().try_for_each(|group| {
        let Some(namespace) = homogeneous_exclusive_namespace(group) else {
            return Ok(());
        };
        let pinned = flat
            .iter()
            .find(|f| namespace_of(f.as_ref()) == Some(namespace));
        match pinned {
            Some(pin) if !group.contains(pin) => Err(CapabilityRuleError::PinExcludedFromFamily {
                namespace: namespace.to_owned(),
                pinned: pin.as_ref().to_owned(),
                family: group.iter().map(|m| m.as_ref().to_owned()).collect(),
            }),
            _ => Ok(()),
        }
    })
}

/// Only a group wholly in one exclusive namespace constrains a flat pin.
fn homogeneous_exclusive_namespace(group: &[CapabilityFlag]) -> Option<&'static str> {
    let first = namespace_of(group.first()?.as_ref())?;
    let namespace = EXCLUSIVE_NAMESPACES.iter().find(|ns| **ns == first)?;
    group
        .iter()
        .all(|m| namespace_of(m.as_ref()) == Some(*namespace))
        .then_some(*namespace)
}

fn resolve_group(group: &Group) -> Result<Vec<CapabilityFlag>, CapabilityRuleError> {
    match group {
        Group::AtLeast { list, floor } => {
            // The tail holds at least the floor; an empty vocabulary fails here.
            let tail = list
                .iter()
                .position(|m| m == floor)
                .and_then(|from| list.get(from..))
                .ok_or_else(|| CapabilityRuleError::RuleTableUnknownFloor {
                    floor: (*floor).to_owned(),
                })?;
            flags(tail)
        }
        Group::AnyOf([]) => Err(CapabilityRuleError::RuleTableEmptyGroup),
        Group::AnyOf(members) => flags(members),
    }
}

fn resolve_groups(groups: &[Group]) -> Result<Vec<Vec<CapabilityFlag>>, CapabilityRuleError> {
    groups.iter().map(resolve_group).collect()
}

fn flag(raw: &str) -> Result<CapabilityFlag, CapabilityRuleError> {
    CapabilityFlag::try_new(raw.to_owned()).map_err(|source| {
        CapabilityRuleError::RuleTableFlagNotCanonical {
            raw: raw.to_owned(),
            source,
        }
    })
}

fn flags(raw: &[&str]) -> Result<Vec<CapabilityFlag>, CapabilityRuleError> {
    raw.iter().map(|s| flag(s)).collect()
}

/// The same sorted, deduplicated normal form `Eligibility` uses.
fn normalized(mut flags: Vec<CapabilityFlag>) -> Vec<CapabilityFlag> {
    flags.sort_unstable_by(|a, b| a.as_ref().cmp(b.as_ref()));
    flags.dedup();
    flags
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use anyhow::Context;
    use rstest::rstest;
    use strum::IntoEnumIterator;

    use pipette_plan_types::RuntimeType;

    use super::*;

    fn requires_of(er: &EffectiveRequirement) -> Vec<String> {
        er.requires.iter().map(|f| f.as_ref().to_owned()).collect()
    }

    /// Whether some `any_of` group of `er` is exactly `expected`, as a set.
    fn has_group(er: &EffectiveRequirement, expected: &[&str]) -> bool {
        let mut want: Vec<&str> = expected.to_vec();
        want.sort_unstable();
        want.dedup();
        er.any_of
            .iter()
            .any(|g| g.iter().map(CapabilityFlag::as_ref).collect::<Vec<_>>() == want)
    }

    fn group_in(er: &EffectiveRequirement, namespace: &str) -> Vec<String> {
        er.any_of
            .iter()
            .find(|g| homogeneous_exclusive_namespace(g) == Some(namespace))
            .map(|g| g.iter().map(|f| f.as_ref().to_owned()).collect())
            .unwrap_or_default()
    }

    // ---- rule primitives (synthetic policies) ----------------------------

    #[test]
    fn requires_injects_all_of() -> anyhow::Result<()> {
        let policy = Policy {
            requires: &["os:linux"],
            ..Policy::EMPTY
        };
        let er = resolve_from_policy(&flags(&["runtime:vllm"])?, &policy)?;
        assert_eq!(requires_of(&er), vec!["os:linux", "runtime:vllm"]);
        assert!(er.any_of.is_empty());
        Ok(())
    }

    #[test]
    fn any_of_group_injected_unconditionally() -> anyhow::Result<()> {
        let policy = Policy {
            any_of: &[Group::AtLeast {
                list: &["chip:a", "chip:b"],
                floor: "chip:a",
            }],
            ..Policy::EMPTY
        };
        let er = resolve_from_policy(&[], &policy)?;
        assert!(has_group(&er, &["chip:a", "chip:b"]));
        Ok(())
    }

    #[test]
    fn any_of_members_injected_as_one_group() -> anyhow::Result<()> {
        let policy = Policy {
            any_of: &[Group::AnyOf(&["cpu_feature:avx512f", "accel:gpu"])],
            ..Policy::EMPTY
        };
        let er = resolve_from_policy(&[], &policy)?;
        assert_eq!(er.any_of.len(), 1);
        assert!(has_group(&er, &["accel:gpu", "cpu_feature:avx512f"]));
        Ok(())
    }

    #[test]
    fn an_empty_any_of_group_is_a_rule_table_error() {
        let policy = Policy {
            any_of: &[Group::AnyOf(&[])],
            ..Policy::EMPTY
        };
        assert_eq!(
            resolve_from_policy(&[], &policy),
            Err(CapabilityRuleError::RuleTableEmptyGroup)
        );
    }

    #[test]
    fn one_of_exactly_one_accepted() -> anyhow::Result<()> {
        let policy = Policy {
            one_of: &["os:ios", "os:macos"],
            ..Policy::EMPTY
        };
        let er = resolve_from_policy(&flags(&["os:ios"])?, &policy)?;
        assert_eq!(requires_of(&er), vec!["os:ios"]);
        Ok(())
    }

    #[rstest]
    #[case::zero_commit(&[], |e: &CapabilityRuleError| {
        matches!(e, CapabilityRuleError::OneOfNoCommit { .. })
    })]
    #[case::two_commits(&["os:ios", "os:macos"], |e: &CapabilityRuleError| {
        matches!(e, CapabilityRuleError::OneOfMultipleCommit { .. })
    })]
    fn one_of_rejects_bad_commit_counts(
        #[case] committed: &[&str],
        #[case] expected: fn(&CapabilityRuleError) -> bool,
    ) -> anyhow::Result<()> {
        let policy = Policy {
            one_of: &["os:ios", "os:macos"],
            ..Policy::EMPTY
        };
        let err = resolve_from_policy(&flags(committed)?, &policy)
            .err()
            .context("a bad one_of commit count must be rejected")?;
        assert!(expected(&err), "got {err:?}");
        Ok(())
    }

    #[test]
    fn one_of_counts_only_author_committed_flags() -> anyhow::Result<()> {
        let policy = Policy {
            requires: &["os:macos"],
            one_of: &["os:ios", "os:macos"],
            ..Policy::EMPTY
        };
        let err = resolve_from_policy(&[], &policy)
            .err()
            .context("an injected one_of member must not satisfy the guardrail")?;
        assert!(
            matches!(err, CapabilityRuleError::OneOfNoCommit { .. }),
            "got {err:?}"
        );
        Ok(())
    }

    #[test]
    fn when_injects_only_on_committed_if() -> anyhow::Result<()> {
        let policy = Policy {
            when: &[When {
                if_present: "os:ios",
                require: &["runtime:llama_cpp"],
                any_of: &[Group::AtLeast {
                    list: &["chip:a", "chip:b"],
                    floor: "chip:a",
                }],
            }],
            ..Policy::EMPTY
        };
        let on = resolve_from_policy(&flags(&["os:ios"])?, &policy)?;
        assert!(requires_of(&on).contains(&"runtime:llama_cpp".to_owned()));
        assert!(has_group(&on, &["chip:a", "chip:b"]));

        let off = resolve_from_policy(&flags(&["os:android"])?, &policy)?;
        assert!(!requires_of(&off).contains(&"runtime:llama_cpp".to_owned()));
        assert!(off.any_of.is_empty());
        Ok(())
    }

    // ---- exclusive-namespace contradictions ------------------------------

    #[rstest]
    #[case::os(&["os:ios", "os:macos"], "os")]
    #[case::device(&["device:iphone17", "device:iphone17pro"], "device")]
    #[case::chip(&["chip:applem1", "chip:applem2"], "chip")]
    #[case::os_version(&["os_version:14", "os_version:15"], "os_version")]
    #[case::ram(&["ram_bytes:8", "ram_bytes:16"], "ram_bytes")]
    fn exclusive_namespace_contradiction_rejected(
        #[case] committed: &[&str],
        #[case] namespace: &str,
    ) -> anyhow::Result<()> {
        let err = resolve_from_policy(&flags(committed)?, &Policy::EMPTY)
            .err()
            .context("two flags in one exclusive namespace must be rejected")?;
        assert!(
            matches!(&err, CapabilityRuleError::ExclusiveNamespaceConflict { namespace: ns, .. } if ns == namespace),
            "got {err:?}"
        );
        Ok(())
    }

    #[test]
    fn injected_flag_contradicting_committed_is_rejected() -> anyhow::Result<()> {
        // MLX-macOS injects `os:macos`.
        let runtime: Runtime = toml::from_str(
            r#"type = "mlx_macos_pipette"
version = "0.20.0"
flavor = "macos-arm64"
source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" }"#,
        )?;
        let err = resolve_effective_requirement(&flags(&["os:android"])?, &runtime)
            .err()
            .context("committed os:android must conflict with injected os:macos")?;
        assert!(
            matches!(&err, CapabilityRuleError::ExclusiveNamespaceConflict { namespace, .. } if namespace == "os"),
            "got {err:?}"
        );
        Ok(())
    }

    #[test]
    fn identical_committed_and_injected_flag_is_accepted() -> anyhow::Result<()> {
        let policy = Policy {
            requires: &["os:linux"],
            ..Policy::EMPTY
        };
        let er = resolve_from_policy(&flags(&["os:linux"])?, &policy)?;
        assert_eq!(requires_of(&er), vec!["os:linux"]);
        Ok(())
    }

    /// `runtime:` and `tag:` are not exclusive: a device advertises several
    /// runtimes and carries several tags.
    #[rstest]
    #[case::runtime(&["runtime:llama_cpp", "runtime:mlx"])]
    #[case::tag(&["tag:lab", "tag:h100"])]
    fn non_reserved_namespace_allows_multiple(#[case] committed: &[&str]) -> anyhow::Result<()> {
        let er = resolve_from_policy(&flags(committed)?, &Policy::EMPTY)?;
        assert_eq!(er.requires.len(), 2);
        Ok(())
    }

    #[test]
    fn any_of_may_repeat_a_namespace() -> anyhow::Result<()> {
        // The exclusive check covers the flat set only; an injected device
        // family is deliberately many `device:` flags.
        let policy = Policy {
            any_of: &[Group::AtLeast {
                list: &["device:a", "device:b", "device:c"],
                floor: "device:a",
            }],
            ..Policy::EMPTY
        };
        let er = resolve_from_policy(&flags(&["os:ios"])?, &policy)?;
        assert!(has_group(&er, &["device:a", "device:b", "device:c"]));
        Ok(())
    }

    #[test]
    fn requirements_are_normalized() -> anyhow::Result<()> {
        let er = resolve_from_policy(
            &flags(&["runtime:b", "runtime:a", "runtime:a"])?,
            &Policy::EMPTY,
        )?;
        assert_eq!(requires_of(&er), vec!["runtime:a", "runtime:b"]);
        Ok(())
    }

    // ---- pins vs. injected families --------------------------------------

    #[rstest]
    #[case::iphone(&["os:ios", "device:iphone12"], "device:iphone12")]
    #[case::ipad(&["os:ipados", "device:ipad(a16)"], "device:ipad(a16)")]
    fn pin_outside_injected_family_is_rejected(
        #[case] committed: &[&str],
        #[case] pinned_device: &str,
    ) -> anyhow::Result<()> {
        let err = resolve_effective_requirement(
            &flags(committed)?,
            &Runtime::AppleFoundation(Default::default()),
        )
        .err()
        .context("a pin the injected family excludes must be rejected")?;
        assert!(
            matches!(
                &err,
                CapabilityRuleError::PinExcludedFromFamily { namespace, pinned, .. }
                    if namespace == "device" && pinned == pinned_device
            ),
            "got {err:?}"
        );
        Ok(())
    }

    #[test]
    fn pin_inside_injected_family_is_accepted() -> anyhow::Result<()> {
        // Narrowing to one supported device is legitimate.
        let er = resolve_effective_requirement(
            &flags(&["os:ios", "device:iphone17pro"])?,
            &Runtime::AppleFoundation(Default::default()),
        )?;
        assert!(requires_of(&er).contains(&"device:iphone17pro".to_owned()));
        Ok(())
    }

    // ---- ordered vocabulary ----------------------------------------------

    #[test]
    fn at_least_group_takes_the_suffix_from_its_floor() -> anyhow::Result<()> {
        let group = resolve_group(&Group::AtLeast {
            list: IPHONES,
            floor: "device:iphone17",
        })?;
        let forms: Vec<&str> = group.iter().map(CapabilityFlag::as_ref).collect();
        assert_eq!(
            forms.first(),
            Some(&"device:iphone17"),
            "the floor leads the family"
        );
        assert!(
            !forms.contains(&"device:iphone16"),
            "devices older than the floor are excluded: {forms:?}"
        );
        assert!(
            forms.contains(&"device:iphone17promax"),
            "everything newer is included: {forms:?}"
        );
        Ok(())
    }

    #[test]
    fn unknown_floor_is_a_rules_table_error() -> anyhow::Result<()> {
        let err = resolve_group(&Group::AtLeast {
            list: IPHONES,
            floor: "device:nosuchphone",
        })
        .err()
        .context("a floor outside its vocabulary must be an error")?;
        assert!(
            matches!(err, CapabilityRuleError::RuleTableUnknownFloor { .. }),
            "got {err:?}"
        );
        Ok(())
    }

    /// An empty vocabulary has no floor, so the misspelled-floor guard catches it.
    #[test]
    fn empty_vocabulary_is_a_rules_table_error() -> anyhow::Result<()> {
        let err = resolve_group(&Group::AtLeast {
            list: &[],
            floor: "chip:applem1",
        })
        .err()
        .context("a group over an empty vocabulary must be an error")?;
        assert!(
            matches!(err, CapabilityRuleError::RuleTableUnknownFloor { .. }),
            "got {err:?}"
        );
        Ok(())
    }

    #[rstest]
    #[case::iphones(IPHONES, "device:iphone15pro", "device:iphonexs")]
    #[case::ipads(IPADS, "device:ipadmini(a17pro)", "device:ipadmini(5thgen)")]
    fn one_vocabulary_keeps_two_floors_consistent(
        #[case] list: &'static [&'static str],
        #[case] afm_floor: &'static str,
        #[case] mlx_floor: &'static str,
    ) -> anyhow::Result<()> {
        let afm = resolve_group(&Group::AtLeast {
            list,
            floor: afm_floor,
        })?;
        let mlx = resolve_group(&Group::AtLeast {
            list,
            floor: mlx_floor,
        })?;
        assert!(
            afm.iter().all(|d| mlx.contains(d)),
            "every AFM-capable device must also be MLX-capable"
        );
        assert!(
            afm.len() < mlx.len(),
            "the AFM family must be the stricter of the two"
        );
        assert_eq!(
            afm.last().map(CapabilityFlag::as_ref),
            mlx.last().map(CapabilityFlag::as_ref),
            "both families must extend to the newest known device"
        );
        Ok(())
    }

    // ---- real Apple-Foundation policy (the acceptance cases) -------------

    #[rstest]
    #[case::ios("os:ios", "device:iphone15pro", "device:iphone15")]
    #[case::ipados("os:ipados", "device:ipadmini(a17pro)", "device:ipad(a16)")]
    fn afm_family_floors_at_a17_pro(
        #[case] os_flag: &str,
        #[case] floor: &str,
        #[case] below_floor: &str,
    ) -> anyhow::Result<()> {
        let er = resolve_effective_requirement(
            &flags(&[os_flag])?,
            &Runtime::AppleFoundation(Default::default()),
        )?;
        let devices = group_in(&er, "device");
        assert!(
            devices.contains(&floor.to_owned()),
            "the floor must be included: {devices:?}"
        );
        assert!(
            !devices.contains(&below_floor.to_owned()),
            "the family must not reach below the floor: {devices:?}"
        );
        Ok(())
    }

    #[test]
    fn afm_macos_variant_has_no_iphone_family() -> anyhow::Result<()> {
        let er = resolve_effective_requirement(
            &flags(&["os:macos"])?,
            &Runtime::AppleFoundation(Default::default()),
        )?;
        assert!(
            group_in(&er, "device").is_empty(),
            "a macOS AFM variant must not gain an iPhone family"
        );
        assert!(has_group(&er, APPLE_SILICON));
        Ok(())
    }

    #[rstest]
    #[case::android(&["os:android"])]
    #[case::two_os(&["os:ios", "os:macos"])]
    fn afm_bad_os_commitment_rejected(#[case] committed: &[&str]) -> anyhow::Result<()> {
        // Not an empty set: that is roster-only, which the guardrail never sees
        // (`a_roster_only_variant_gets_no_injection`).
        let err = resolve_effective_requirement(
            &flags(committed)?,
            &Runtime::AppleFoundation(Default::default()),
        )
        .err()
        .context("a bad AFM OS commitment must be rejected")?;
        assert!(
            matches!(
                err,
                CapabilityRuleError::OneOfNoCommit { .. }
                    | CapabilityRuleError::OneOfMultipleCommit { .. }
            ),
            "got {err:?}"
        );
        Ok(())
    }

    // ---- other real policies ---------------------------------------------

    /// The client side of this match is tested in `pipette-core`'s `matching`.
    #[rstest]
    #[case::docker_vllm(
        r#"type = "docker_vllm"
image_name = "vllm/vllm-openai"
image_tag = "v0.21.0"
flavor = "nvidia_gpu""#
    )]
    #[case::docker_sglang(
        r#"type = "docker_sglang"
image_name = "lmsysorg/sglang"
image_tag = "v0.5.15-cu130"
flavor = "nvidia_gpu""#
    )]
    #[case::uv_vllm(
        r#"type = "uv_vllm"
server_version = "0.10.0"
build = "cu121"
python_version = "3.12"
source = { type = "pip_requirements_text", contents = "vllm==0.10.0" }"#
    )]
    #[case::uv_sglang(
        r#"type = "uv_sglang"
server_version = "0.5.15"
build = "cu130"
python_version = "3.12"
source = { type = "pip_requirements_text", contents = "sglang==0.5.15" }"#
    )]
    fn server_runtime_injects_a_gpu_or_avx512_and_no_os(
        #[case] runtime: &str,
    ) -> anyhow::Result<()> {
        let runtime: Runtime = toml::from_str(runtime)?;
        let er = resolve_effective_requirement(&flags(&["tag:lab-h100-eval"])?, &runtime)?;
        assert_eq!(requires_of(&er), vec!["tag:lab-h100-eval"]);
        assert_eq!(er.any_of.len(), 1, "got {er:?}");
        assert!(
            has_group(&er, &["accel:gpu", "cpu_feature:avx512f"]),
            "got {er:?}"
        );
        Ok(())
    }

    #[rstest]
    #[case::mlx_macos(
        r#"type = "mlx_macos_pipette"
version = "0.20.0"
flavor = "macos-arm64"
source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" }"#
    )]
    fn desktop_macos_runtime_injects_os_and_apple_silicon(
        #[case] runtime: &str,
    ) -> anyhow::Result<()> {
        let runtime: Runtime = toml::from_str(runtime)?;
        let er = resolve_effective_requirement(&flags(&["os:macos"])?, &runtime)?;
        assert!(requires_of(&er).contains(&"os:macos".to_owned()));
        assert!(has_group(&er, APPLE_SILICON));
        Ok(())
    }

    /// The policy attaches to the author's `requires`; with none, nothing is
    /// injected — not even where the policy demands a platform commitment.
    #[rstest]
    #[case::mlx_macos(
        r#"type = "mlx_macos_pipette"
version = "0.20.0"
flavor = "macos-arm64"
source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" }"#
    )]
    #[case::uv_sglang(
        r#"type = "uv_sglang"
server_version = "0.4.0"
build = "cu121"
python_version = "3.12"
source = { type = "pip_requirements_text", contents = "sglang==0.4.0" }"#
    )]
    #[case::apple_foundation(r#"type = "apple_foundation""#)]
    fn a_roster_only_variant_gets_no_injection(#[case] runtime_toml: &str) -> anyhow::Result<()> {
        let runtime: Runtime = toml::from_str(runtime_toml)?;
        let er = resolve_effective_requirement(&[], &runtime)?;
        assert_eq!(er, EffectiveRequirement::NONE, "got {er:?}");
        Ok(())
    }

    fn mlx_ios_runtime() -> anyhow::Result<Runtime> {
        Ok(toml::from_str(
            r#"type = "mlx_ios_pipette"
flavor = "ios-arm64"
packages = { mlx_swift = { repository_url = "github.com/ml-explore/mlx-swift", version = "1" }, mlx_swift_lm = { repository_url = "github.com/ml-explore/mlx-swift-examples", version = "1" }, swift_transformers = { repository_url = "github.com/huggingface/swift-transformers", version = "1" } }"#,
        )?)
    }

    #[rstest]
    #[case::ios("os:ios", "device:iphonexs")]
    #[case::ipados("os:ipados", "device:ipadmini(5thgen)")]
    fn mlx_ios_family_reaches_back_to_a12(
        #[case] os_flag: &str,
        #[case] floor: &str,
    ) -> anyhow::Result<()> {
        let er = resolve_effective_requirement(&flags(&[os_flag])?, &mlx_ios_runtime()?)?;
        assert!(requires_of(&er).contains(&os_flag.to_owned()));
        let devices = group_in(&er, "device");
        assert!(
            devices.contains(&floor.to_owned()),
            "the A12 floor must be included: {devices:?}"
        );
        Ok(())
    }

    // ---- iPadOS ----------------------------------------------------------

    /// An iPad is never held to iPhone hardware.
    #[rstest]
    #[case::apple_foundation(r#"type = "apple_foundation""#)]
    #[case::mlx_ios(
        r#"type = "mlx_ios_pipette"
flavor = "ios-arm64"
packages = { mlx_swift = { repository_url = "github.com/ml-explore/mlx-swift", version = "1" }, mlx_swift_lm = { repository_url = "github.com/ml-explore/mlx-swift-examples", version = "1" }, swift_transformers = { repository_url = "github.com/huggingface/swift-transformers", version = "1" } }"#
    )]
    fn ipados_variant_gains_the_ipad_family(#[case] runtime_toml: &str) -> anyhow::Result<()> {
        let runtime: Runtime = toml::from_str(runtime_toml)?;
        let er = resolve_effective_requirement(&flags(&["os:ipados"])?, &runtime)?;
        assert!(requires_of(&er).contains(&"os:ipados".to_owned()));
        let devices = group_in(&er, "device");
        assert!(
            devices.iter().all(|d| IPADS.contains(&d.as_str())),
            "every family member must be an iPad: {devices:?}"
        );
        assert!(
            devices.contains(&"device:ipadpro13-inch(m5)".to_owned()),
            "the family must reach the newest iPad: {devices:?}"
        );
        Ok(())
    }

    /// [`IPADS`] orders the whole A line ahead of the whole M line, so the A17
    /// Pro floor admits every M-series iPad.
    #[test]
    fn afm_ipados_family_admits_the_oldest_m_series_ipad() -> anyhow::Result<()> {
        let er = resolve_effective_requirement(
            &flags(&["os:ipados"])?,
            &Runtime::AppleFoundation(Default::default()),
        )?;
        let devices = group_in(&er, "device");
        assert!(
            devices.contains(&"device:ipadpro11-inch(3rdgen)".to_owned()),
            "the oldest M-series iPad qualifies: {devices:?}"
        );
        Ok(())
    }

    fn llamacpp_ios_runtime() -> anyhow::Result<Runtime> {
        Ok(toml::from_str(
            r#"type = "llamacpp_ios_pipette"
repository_url = "github.com/ggml-org/llama.cpp"
version = "b10216"
flavor = "ios-arm64""#,
        )?)
    }

    fn onnx_genai_ios_runtime() -> anyhow::Result<Runtime> {
        Ok(toml::from_str(
            r#"type = "onnx_genai_ios_pipette"
flavor = "ios-arm64"
packages = { onnxruntime = { repository_url = "github.com/microsoft/onnxruntime", repository_version = "v1.23.0" }, onnxruntime_genai = { repository_url = "github.com/microsoft/onnxruntime-genai", repository_version = "v0.15.2" } }"#,
        )?)
    }

    /// An iPad runs the same binary as an iPhone, so both platforms are
    /// authorable and neither carries a device floor.
    #[rstest]
    #[case::ios("os:ios")]
    #[case::ipados("os:ipados")]
    fn llamacpp_ios_accepts_either_apple_mobile_platform(
        #[case] os_flag: &str,
    ) -> anyhow::Result<()> {
        let er = resolve_effective_requirement(&flags(&[os_flag])?, &llamacpp_ios_runtime()?)?;
        assert_eq!(requires_of(&er), vec![os_flag.to_owned()]);
        assert!(er.any_of.is_empty(), "no device floor: {:?}", er.any_of);
        Ok(())
    }

    /// The zero-commit case uses a third platform: an empty set is roster-only,
    /// which the guardrail never sees.
    #[rstest]
    #[case::no_commit(&["os:android"], |e: &CapabilityRuleError| {
        matches!(e, CapabilityRuleError::OneOfNoCommit { .. })
    })]
    #[case::both_platforms(&["os:ios", "os:ipados"], |e: &CapabilityRuleError| {
        matches!(e, CapabilityRuleError::OneOfMultipleCommit { .. })
    })]
    fn an_apple_mobile_runtime_requires_exactly_one_platform(
        #[case] committed: &[&str],
        #[case] expected: fn(&CapabilityRuleError) -> bool,
        #[values(mlx_ios_runtime, llamacpp_ios_runtime, onnx_genai_ios_runtime)]
        runtime: fn() -> anyhow::Result<Runtime>,
    ) -> anyhow::Result<()> {
        let err = resolve_effective_requirement(&flags(committed)?, &runtime()?)
            .err()
            .context("a bad Apple-mobile-platform commitment must be rejected")?;
        assert!(expected(&err), "got {err:?}");
        Ok(())
    }

    #[test]
    fn desktop_llamacpp_injects_nothing() -> anyhow::Result<()> {
        let runtime: Runtime = toml::from_str(
            r#"type = "llamacpp_cli_stock_tools"
source = "github_release"
repository_url = "github.com/ggml-org/llama.cpp"
version = "b5000"
flavor = "macos-arm64""#,
        )?;
        let er = resolve_effective_requirement(&flags(&["os:windows"])?, &runtime)?;
        assert_eq!(requires_of(&er), vec!["os:windows"]);
        assert!(er.any_of.is_empty());
        Ok(())
    }

    // ---- ONNX Runtime GenAI: one runtime, two policies -------------------

    fn onnx_genai_runtime(execution_provider: &str) -> anyhow::Result<Runtime> {
        Ok(toml::from_str(&format!(
            r#"type = "uv_onnx_genai"
server_version = "0.16.0"
execution_provider = "{execution_provider}"
python_version = "3.11"
source = {{ type = "pip_requirements_text", contents = "onnxruntime-genai==0.16.0" }}"#
        ))?)
    }

    /// The execution provider decides the policy: CPU adds nothing, and CUDA
    /// adds `accel:gpu` but no `os:`, keeping any `os:` the author wrote.
    #[rstest]
    #[case::cpu_on_macos("cpu", "os:macos", vec!["os:macos"])]
    #[case::cuda_by_tag("cuda", "tag:lab-h100-eval", vec!["accel:gpu", "tag:lab-h100-eval"])]
    #[case::cuda_on_ubuntu("cuda", "os:ubuntu", vec!["accel:gpu", "os:ubuntu"])]
    #[case::cuda_on_windows(
        "cuda",
        "os:microsoftwindows11pro",
        vec!["accel:gpu", "os:microsoftwindows11pro"]
    )]
    fn an_onnx_provider_injects_its_own_hardware_floor(
        #[case] execution_provider: &str,
        #[case] committed: &str,
        #[case] expected: Vec<&str>,
    ) -> anyhow::Result<()> {
        let er = resolve_effective_requirement(
            &flags(&[committed])?,
            &onnx_genai_runtime(execution_provider)?,
        )?;
        assert_eq!(requires_of(&er), expected);
        assert!(er.any_of.is_empty(), "no injected group: {:?}", er.any_of);
        Ok(())
    }

    // ---- OVMS: the flavor pins no OS --------------------------------------

    fn ovms_runtime(flavor: &str) -> anyhow::Result<Runtime> {
        Ok(toml::from_str(&format!(
            r#"type = "ovms"
source = "github_release"
repository_url = "github.com/openvinotoolkit/model_server"
repository_version = "v2026.4.0"
flavor = "{flavor}""#
        ))?)
    }

    /// Each archive runs on one OS, but no client reports `os:linux` or
    /// `os:windows`, so the platform is the author's to state.
    #[rstest]
    #[case::windows("windows-x64")]
    #[case::ubuntu22("ubuntu22-x64")]
    #[case::ubuntu24("ubuntu24-x64")]
    #[case::redhat("redhat-x64")]
    #[case::custom("acme-arm64")]
    fn an_ovms_flavor_injects_no_platform(#[case] flavor: &str) -> anyhow::Result<()> {
        let er = resolve_effective_requirement(&flags(&["runtime:ovms"])?, &ovms_runtime(flavor)?)?;
        assert_eq!(requires_of(&er), vec!["runtime:ovms"]);
        assert!(er.any_of.is_empty(), "no injected group: {:?}", er.any_of);
        Ok(())
    }

    // ---- TensorRT Edge-LLM: a Jetson GPU ---------------------------------

    /// The archive needs a Tegra GPU, so both flags join whatever the author
    /// wrote; a tag narrows the fleet further.
    #[rstest]
    #[case::by_runtime(&["runtime:trt_edgellm"], vec!["accel:gpu", "cpu_vendor:nvidia", "runtime:trt_edgellm"])]
    #[case::by_tag(&["tag:lab-orin"], vec!["accel:gpu", "cpu_vendor:nvidia", "tag:lab-orin"])]
    #[case::restated(&["accel:gpu"], vec!["accel:gpu", "cpu_vendor:nvidia"])]
    fn a_trt_edgellm_runtime_requires_a_jetson_gpu(
        #[case] committed: &[&str],
        #[case] expected: Vec<&str>,
    ) -> anyhow::Result<()> {
        let runtime: Runtime = toml::from_str(
            r#"type = "trt_edgellm"
source = "relative_dir"
dir = "blobs"
flavor = "jetson-orin-cuda-13""#,
        )?;
        let er = resolve_effective_requirement(&flags(committed)?, &runtime)?;
        assert_eq!(requires_of(&er), expected);
        assert!(er.any_of.is_empty(), "no injected group: {:?}", er.any_of);
        Ok(())
    }

    // ---- plan-level validation over a parsed SchedulerPlan ---------------

    /// The `clients_only` case takes no injection, so the platform guardrail has
    /// nothing to ask.
    #[rstest]
    #[case::requires_ios(r#"requires = ["os:ios"]"#)]
    #[case::clients_only(r#"clients  = ["ev1_abc"]"#)]
    fn validate_accepts_a_well_formed_afm_plan(#[case] eligibility: &str) -> anyhow::Result<()> {
        let plan = SchedulerPlan::parse(&format!(
            r#"benchmarks = ["decode_throughput_512_100"]
[[variants]]
{eligibility}
models   = [{{ type = "apple_foundation_text" }}]
runtimes = [{{ type = "apple_foundation" }}]"#
        ))?;
        assert!(validate_capability_rules(&plan)?.is_empty());
        Ok(())
    }

    #[test]
    fn validate_rejects_afm_variant_with_a_bad_os_and_names_the_variant() -> anyhow::Result<()> {
        let plan = SchedulerPlan::parse(
            r#"benchmarks = ["decode_throughput_512_100"]
[[variants]]
requires = ["os:android"]
models   = [{ type = "apple_foundation_text" }]
runtimes = [{ type = "apple_foundation" }]"#,
        )?;
        let err = validate_capability_rules(&plan)
            .err()
            .context("an AFM variant with no Apple OS must be rejected at generation")?;
        assert!(
            err.to_string().contains("variant 0"),
            "missing variant context: {err}"
        );
        assert!(
            matches!(&err.rule, CapabilityRuleError::OneOfNoCommit { .. }),
            "expected a typed OneOfNoCommit cause, got {err:?}"
        );
        Ok(())
    }

    // ---- exclusion clauses ----------------------------------------------

    /// A one-variant MLX plan; the runtime injects `os:macos` and the `chip:`
    /// family, so a case states only the clause under test.
    fn mlx_plan(lines: &str) -> String {
        format!(
            r#"benchmarks = ["decode_throughput_512_100"]
[[variants]]
{lines}
models   = [{{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }}]
runtimes = [{{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = {{ type = "pip_requirements_text", contents = "mlx-lm==0.20.0" }} }}]"#
        )
    }

    /// A one-variant llama.cpp plan. That runtime's policy is empty, so the
    /// effective requirement is whatever the author wrote and nothing more.
    fn llamacpp_plan(lines: &str) -> String {
        format!(
            r#"benchmarks = ["decode_throughput_512_100"]
[[variants]]
{lines}
models   = [{{ type = "gguf_text", source = "huggingface", org = "org", repo_name = "repo", path = "Q4_K_M.gguf" }}]
runtimes = [{{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b5000", flavor = "macos-arm64" }}]"#
        )
    }

    /// Every case has a dead `clients` roster, which makes a dead capability
    /// path fatal.
    #[rstest]
    #[case::excludes_a_required_flag(
        "requires = [\"os:macos\"]\nexcludes = [\"os:macos\"]",
        (|e| matches!(e, CapabilityRuleError::ExcludesRequiredFlag { .. }))
            as fn(&CapabilityRuleError) -> bool
    )]
    #[case::not_all_of_within_requires(
        "requires = [\"os:macos\"]\nnot_all_of = [[\"os:macos\"]]",
        (|e| matches!(e, CapabilityRuleError::NotAllOfWithinRequires { .. }))
            as fn(&CapabilityRuleError) -> bool
    )]
    #[case::vacuous_exclusion_group(
        "requires = [\"os:macos\"]\nnot_all_of = [[\"chip:applem1\", \"chip:applem2\"]]",
        (|e| matches!(e, CapabilityRuleError::VacuousExclusionGroup { .. }))
            as fn(&CapabilityRuleError) -> bool
    )]
    #[case::excludes_every_client_version(
        "requires = [\"os:macos\"]\nclient_versions = [\"2.0\", \"1.0\"]\nexcludes = [\"client_version:1.0\", \"client_version:2.0\"]",
        (|e| matches!(e, CapabilityRuleError::ExcludesCoversAnyOfGroup { .. }))
            as fn(&CapabilityRuleError) -> bool
    )]
    // A `:required` mark comes off before the server matches, so the rules
    // read the plain flag.
    #[case::excludes_every_required_client_version(
        "requires = [\"os:macos\"]\nclient_versions = [\"1.0:required\"]\nexcludes = [\"client_version:1.0\"]",
        (|e| matches!(e, CapabilityRuleError::ExcludesCoversAnyOfGroup { .. }))
            as fn(&CapabilityRuleError) -> bool
    )]
    #[case::excludes_a_required_tag(
        "requires = [\"os:macos\", \"tag:lab:required\"]\nexcludes = [\"tag:lab\"]",
        (|e| matches!(e, CapabilityRuleError::ExcludesRequiredFlag { .. }))
            as fn(&CapabilityRuleError) -> bool
    )]
    fn validate_rejects_an_unmatchable_exclusion(
        #[case] lines: &str,
        #[case] is_expected: fn(&CapabilityRuleError) -> bool,
    ) -> anyhow::Result<()> {
        let plan = SchedulerPlan::parse(&mlx_plan(lines))?;
        let err = validate_capability_rules(&plan)
            .err()
            .context("expected the exclusion to be rejected at generation")?;
        assert!(
            err.to_string().contains("variant 0"),
            "missing variant context: {err}"
        );
        assert!(is_expected(&err.rule), "unexpected typed cause: {err:?}");
        Ok(())
    }

    #[test]
    fn excludes_covering_the_injected_family_is_rejected() -> anyhow::Result<()> {
        // The author never writes `any_of`, so the covered family is invisible in
        // the plan file.
        let excludes = APPLE_SILICON
            .iter()
            .map(|chip| format!("\"{chip}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let plan = SchedulerPlan::parse(&mlx_plan(&format!(
            "requires = [\"os:macos\"]\nexcludes = [{excludes}]"
        )))?;
        let err = validate_capability_rules(&plan)
            .err()
            .context("expected a covered `any_of` group to be rejected")?;
        assert!(
            matches!(
                &err.rule,
                CapabilityRuleError::ExcludesCoversAnyOfGroup { .. }
            ),
            "expected a typed ExcludesCoversAnyOfGroup cause, got {err:?}"
        );
        Ok(())
    }

    /// A roster-only variant takes no injection on any runtime, MLX's `os:macos`
    /// included, so nothing is left once `exclude_clients` covers the roster.
    #[rstest]
    #[case::llamacpp(llamacpp_plan as fn(&str) -> String)]
    #[case::mlx(mlx_plan as fn(&str) -> String)]
    fn a_wholly_excluded_roster_with_no_requires_is_rejected(
        #[case] plan: fn(&str) -> String,
    ) -> anyhow::Result<()> {
        let plan = SchedulerPlan::parse(&plan(
            "clients = [\"ev1_a\"]\nexclude_clients = [\"ev1_a\"]",
        ))?;
        let err = validate_capability_rules(&plan)
            .err()
            .context("expected a wholly excluded roster to be rejected")?;
        assert!(
            matches!(&err.rule, CapabilityRuleError::RosterWhollyExcluded),
            "expected a typed RosterWhollyExcluded cause, got {err:?}"
        );
        Ok(())
    }

    fn assert_one_warning(plan: &SchedulerPlan, needles: &[&str]) -> anyhow::Result<()> {
        let warnings = validate_capability_rules(plan)?;
        assert_eq!(warnings.len(), 1, "expected one warning: {warnings:?}");
        let warning = &warnings[0].message;
        needles.iter().for_each(|needle| {
            assert!(
                warning.contains(needle),
                "warning missing {needle:?}: {warning}"
            );
        });
        Ok(())
    }

    #[test]
    fn a_live_roster_turns_a_dead_capability_path_into_a_warning() -> anyhow::Result<()> {
        // `pipette-mgmt` ingests this body (the roster is live), so refusing it
        // here would reject a plan the server accepts.
        let plan = SchedulerPlan::parse(&mlx_plan(
            "requires = [\"os:macos\"]\nclients = [\"ev1_live\"]\nexcludes = [\"os:macos\"]",
        ))?;
        assert_one_warning(
            &plan,
            &[
                "variant 0",
                "`excludes` names",
                "at most the 1 client listed",
            ],
        )
    }

    #[test]
    fn a_cancelled_roster_warns_even_though_requires_still_matches() -> anyhow::Result<()> {
        // Two runtimes, one warning: the author is told once per variant.
        let plan = SchedulerPlan::parse(
            r#"benchmarks = ["decode_throughput_512_100"]
[[variants]]
requires = ["os:macos"]
clients = ["ev1_a"]
exclude_clients = ["ev1_a"]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }]
runtimes = [
  { type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } },
  { type = "mlx_macos_pipette", version = "0.21.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.21.0" } },
]"#,
        )?;
        assert_one_warning(
            &plan,
            &["variant 0", "`exclude_clients` denies", "adds nobody"],
        )
    }

    /// The roster ignores `client_versions`, so a variant that lists both runs
    /// on a rostered client at any build, and the author is told once.
    #[rstest]
    #[case::beside_requires(mlx_plan, "requires = [\"os:macos\"]\nclients = [\"ev1_a\"]")]
    #[case::roster_only(llamacpp_plan, "clients = [\"ev1_a\"]")]
    fn a_roster_beside_client_versions_warns(
        #[case] plan: fn(&str) -> String,
        #[case] lines: &str,
    ) -> anyhow::Result<()> {
        let plan = SchedulerPlan::parse(&plan(&format!("{lines}\nclient_versions = [\"1.0\"]")))?;
        assert_one_warning(
            &plan,
            &[
                "variant 0",
                "`client_versions` narrows only",
                "whatever version",
            ],
        )
    }

    /// The second warning is the one the roster earns beside `client_versions`.
    #[test]
    fn a_live_roster_turns_every_excluded_version_into_a_warning() -> anyhow::Result<()> {
        let plan = SchedulerPlan::parse(&mlx_plan(
            "requires = [\"os:macos\"]\nclients = [\"ev1_live\"]\nclient_versions = [\"1.0\"]\nexcludes = [\"client_version:1.0\"]",
        ))?;
        let warnings = validate_capability_rules(&plan)?;
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        let dead = warnings
            .iter()
            .find(|w| {
                w.message
                    .contains("`any_of` group [\"client_version:1.0\"]")
            })
            .context("a warning for the excluded versions")?;
        assert!(
            dead.message.contains("at most the 1 client listed"),
            "{}",
            dead.message
        );
        Ok(())
    }

    /// Shapes a nearby rule must not reject, so the rules stay narrower than the
    /// clause they police.
    #[rstest]
    #[case::repeated_flag_in_a_group(
        mlx_plan,
        "requires = [\"os:macos\"]\nnot_all_of = [[\"chip:applem1\", \"chip:applem1\"]]"
    )]
    // `runtime:` and `tag:` are not exclusive: a client carries many.
    #[case::two_unreserved_flags_in_a_group(
        mlx_plan,
        "requires = [\"os:macos\"]\nnot_all_of = [[\"runtime:llama_cpp\", \"runtime:mlx\"]]"
    )]
    #[case::two_tags_in_a_group(
        mlx_plan,
        "requires = [\"os:macos\"]\nnot_all_of = [[\"tag:lab\", \"tag:busy\"]]"
    )]
    // One reserved flag per group: the vacuity rule reads a group, not the list.
    #[case::one_reserved_flag_per_group(
        mlx_plan,
        "requires = [\"os:macos\"]\nnot_all_of = [[\"chip:applem1\"], [\"chip:applem2\"]]"
    )]
    #[case::an_unrelated_denial(
        mlx_plan,
        "requires = [\"os:macos\"]\nexclude_clients = [\"ev1_dead\"]"
    )]
    // llama.cpp injects nothing, so the empty capability path is no fault.
    #[case::clients_only_on_an_empty_policy(llamacpp_plan, "clients = [\"ev1_a\"]")]
    #[case::one_client_version_left(
        mlx_plan,
        "requires = [\"os:macos\"]\nclient_versions = [\"1.0\", \"2.0\"]\nexcludes = [\"client_version:1.0\"]"
    )]
    fn validate_accepts_an_effective_exclusion(
        #[case] plan: fn(&str) -> String,
        #[case] lines: &str,
    ) -> anyhow::Result<()> {
        let plan = SchedulerPlan::parse(&plan(lines))?;
        assert!(
            validate_capability_rules(&plan)?.is_empty(),
            "a well-formed exclusion should warn about nothing"
        );
        Ok(())
    }

    // ---- drift guard: the whole table stays well formed ------------------

    /// One runtime per kind and per named ONNX provider: the rules read the whole
    /// [`Runtime`]. The two `sample_runtimes_cover_*` tests keep it complete.
    const SAMPLE_RUNTIMES: &[&str] = &[
        r#"type = "apple_foundation""#,
        r#"type = "mlx_macos_pipette"
version = "0.31.3"
flavor = "macos-arm64"
source = { type = "pip_requirements_text", contents = "mlx-lm==0.31.3" }"#,
        r#"type = "mlx_ios_pipette"
flavor = "ios-arm64"
packages = { mlx_swift = { repository_url = "github.com/ml-explore/mlx-swift", version = "1" }, mlx_swift_lm = { repository_url = "github.com/ml-explore/mlx-swift-examples", version = "1" }, swift_transformers = { repository_url = "github.com/huggingface/swift-transformers", version = "1" } }"#,
        r#"type = "docker_vllm"
image_name = "vllm/vllm-openai"
image_tag = "v0.21.0"
flavor = "nvidia_gpu""#,
        r#"type = "docker_sglang"
image_name = "lmsysorg/sglang"
image_tag = "v0.5.15-cu130"
flavor = "nvidia_gpu""#,
        r#"type = "uv_vllm"
server_version = "0.10.0"
build = "cu121"
python_version = "3.12"
source = { type = "pip_requirements_text", contents = "vllm==0.10.0" }"#,
        r#"type = "uv_sglang"
server_version = "0.5.15"
build = "cu130"
python_version = "3.12"
source = { type = "pip_requirements_text", contents = "sglang==0.5.15" }"#,
        r#"type = "uv_openvino"
server_version = "2026.2.1"
python_version = "3.11"
source = { type = "pip_requirements_text", contents = "openvino-genai==2026.2.1.0" }"#,
        r#"type = "uv_onnx_genai"
server_version = "0.16.0"
execution_provider = "cpu"
python_version = "3.11"
source = { type = "pip_requirements_text", contents = "onnxruntime-genai==0.16.0" }"#,
        r#"type = "uv_onnx_genai"
server_version = "0.16.0"
execution_provider = "cuda"
python_version = "3.11"
source = { type = "pip_requirements_text", contents = "onnxruntime-genai-cuda==0.16.0" }"#,
        r#"type = "uv_onnx_genai"
server_version = "0.14.1"
execution_provider = "directml"
python_version = "3.11"
source = { type = "pip_requirements_text", contents = "onnxruntime-genai-directml==0.14.1" }"#,
        r#"type = "uv_onnx_genai"
server_version = "0.16.0"
execution_provider = "winml"
python_version = "3.11"
source = { type = "pip_requirements_text", contents = "onnxruntime-genai==0.16.0" }"#,
        r#"type = "ovms"
source = "github_release"
repository_url = "github.com/openvinotoolkit/model_server"
repository_version = "v2026.4.0"
flavor = "windows-x64""#,
        r#"type = "trt_edgellm"
source = "relative_dir"
dir = "blobs"
flavor = "jetson-orin-cuda-13""#,
        r#"type = "llamacpp_cli_stock_tools"
source = "github_release"
repository_url = "github.com/ggml-org/llama.cpp"
version = "b5000"
flavor = "macos-arm64""#,
        r#"type = "llamacpp_apk_pipette"
repository_url = "github.com/ggml-org/llama.cpp"
version = "b9050"
flavor = "android-arm64-v8""#,
        r#"type = "llamacpp_ios_pipette"
repository_url = "github.com/ggml-org/llama.cpp"
version = "b10216"
flavor = "ios-arm64""#,
        r#"type = "onnx_genai_ios_pipette"
flavor = "ios-arm64"
packages = { onnxruntime = { repository_url = "github.com/microsoft/onnxruntime", repository_version = "v1.23.0" }, onnxruntime_genai = { repository_url = "github.com/microsoft/onnxruntime-genai", repository_version = "v0.15.2" } }"#,
    ];

    fn sample_runtimes() -> anyhow::Result<Vec<Runtime>> {
        SAMPLE_RUNTIMES
            .iter()
            .map(|source| {
                toml::from_str::<Runtime>(source).with_context(|| format!("parsing {source}"))
            })
            .collect()
    }

    /// A kind missing from the samples skips the well-formedness guard below.
    #[test]
    fn sample_runtimes_cover_every_runtime_kind() -> anyhow::Result<()> {
        let sampled: HashSet<RuntimeType> =
            sample_runtimes()?.iter().map(RuntimeType::of).collect();
        let declared: HashSet<RuntimeType> = RuntimeType::iter().collect();
        let missing: Vec<String> = declared
            .difference(&sampled)
            .map(RuntimeType::to_string)
            .collect();
        anyhow::ensure!(
            missing.is_empty(),
            "these runtime kinds have no sample runtime: {missing:?}"
        );
        Ok(())
    }

    /// `policy_for` reads the ONNX provider, so each named one needs a sample.
    /// `Custom` is left out, as by `known()`: pipette does not own its spelling.
    #[test]
    fn sample_runtimes_cover_every_onnx_execution_provider() -> anyhow::Result<()> {
        let sampled: HashSet<OnnxExecutionProvider> = sample_runtimes()?
            .into_iter()
            .filter_map(|runtime| {
                if let Runtime::UvOnnxGenai(u) = runtime {
                    Some(u.execution_provider)
                } else {
                    None
                }
            })
            .collect();
        let missing: Vec<String> = OnnxExecutionProvider::known()
            .filter(|provider| !sampled.contains(provider))
            .map(|provider| provider.to_string())
            .collect();
        anyhow::ensure!(
            missing.is_empty(),
            "these ONNX execution providers have no sample runtime: {missing:?}"
        );
        Ok(())
    }

    #[test]
    fn rules_table_is_well_formed() -> anyhow::Result<()> {
        sample_runtimes()?
            .iter()
            .try_for_each(|rt| -> anyhow::Result<()> {
                let policy = policy_for(rt);
                flags(policy.requires)?;
                flags(policy.one_of)?;
                resolve_groups(policy.any_of)?;
                policy.when.iter().try_for_each(|w| -> anyhow::Result<()> {
                    flag(w.if_present)?;
                    flags(w.require)?;
                    resolve_groups(w.any_of)?;
                    Ok(())
                })?;

                // Exclusive namespaces make checking `one_of` before `when` safe: a
                // `when`-injected duplicate trips `check_no_exclusive_conflict`.
                policy.one_of.iter().try_for_each(|member| {
                    let namespace =
                        namespace_of(member).context("a one_of member must be namespaced")?;
                    anyhow::ensure!(
                        EXCLUSIVE_NAMESPACES.contains(&namespace),
                        "one_of member {member:?} is not in an exclusive namespace, so a \
                     `when` injection could defeat the exactly-one guardrail"
                    );
                    Ok(())
                })
            })
    }

    #[rstest]
    #[case::iphones(IPHONES)]
    #[case::ipads(IPADS)]
    fn ordered_vocabularies_have_no_duplicates(#[case] vocabulary: &[&str]) -> anyhow::Result<()> {
        // `Group::AtLeast` resolves a floor by first match, so a duplicate
        // entry would silently make the family's lower bound ambiguous.
        let mut seen = HashSet::new();
        vocabulary.iter().try_for_each(|entry| {
            anyhow::ensure!(seen.insert(*entry), "duplicate vocabulary entry {entry:?}");
            Ok(())
        })
    }

    /// The rules' reserved namespaces are the server's. The copy exists because
    /// this crate names no server crate; the test is what keeps it one.
    #[test]
    fn exclusive_namespaces_match_the_servers() {
        // Sorted, so a failure lists both sides in the same order.
        let ours: BTreeSet<&str> = EXCLUSIVE_NAMESPACES.iter().copied().collect();
        let servers: BTreeSet<&str> = pipette_core::client::RESERVED_CAPABILITY_NAMESPACES
            .iter()
            .copied()
            .collect();
        assert_eq!(ours, servers);
    }

    #[test]
    fn the_required_mark_is_the_servers() {
        assert_eq!(REQUIRED_SUFFIX, pipette_core::matching::REQUIRED_SUFFIX);
        assert!(
            MARKED_NAMESPACES
                .iter()
                .all(|ns| pipette_core::matching::is_routing_flag(&format!("{ns}:x")))
        );
    }

    // ---- drift guard: the vocabularies match Probe.swift ------------

    /// The snapshot of `Probe.swift`, the authority both device vocabularies
    /// mirror. The comment above `IPHONES` says when to refresh it.
    fn device_probe_source() -> anyhow::Result<String> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/Probe.swift");
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
    }

    /// A map this cannot find is an error, so a rename fails rather than
    /// passing vacuously.
    fn swift_map(source: &str, name: &str) -> anyhow::Result<Vec<(String, String)>> {
        let opening = format!("static let {name}: [String: String] = [");
        let (_, body) = source
            .split_once(&opening)
            .with_context(|| format!("Probe.swift declares no {name}"))?;
        let (body, _) = body
            .split_once("\n    ]")
            .with_context(|| format!("{name} has no closing bracket"))?;
        let pairs: Vec<(String, String)> = body
            .lines()
            .filter_map(|line| {
                // A map line is `"key": "value",`; a comment line has no pair
                // of quoted tokens and drops out here.
                let mut quoted = line.split('"').skip(1).step_by(2);
                Some((quoted.next()?.to_owned(), quoted.next()?.to_owned()))
            })
            .collect();
        anyhow::ensure!(!pairs.is_empty(), "{name} parsed to no entries");
        Ok(pairs)
    }

    fn swift_device_flags(source: &str, prefix: &str) -> anyhow::Result<HashSet<String>> {
        Ok(swift_map(source, "modelNameMapping")?
            .into_iter()
            .filter(|(identifier, _)| identifier.starts_with(prefix))
            .map(|(_, name)| format!("device:{}", pipette_core::client::slugify(&name)))
            .collect())
    }

    /// A device flag matches only the exact string a client reports, so the
    /// vocabulary and the Swift map must name the same set.
    #[rstest]
    #[case::iphones("iPhone", IPHONES)]
    #[case::ipads("iPad", IPADS)]
    fn vocabularies_match_the_swift_device_map(
        #[case] prefix: &str,
        #[case] vocabulary: &[&str],
    ) -> anyhow::Result<()> {
        let source = device_probe_source()?;
        let reported = swift_device_flags(&source, prefix)?;
        let listed: HashSet<String> = vocabulary.iter().map(|e| (*e).to_owned()).collect();
        let mut unlisted: Vec<&String> = reported.difference(&listed).collect();
        let mut unreported: Vec<&String> = listed.difference(&reported).collect();
        unlisted.sort_unstable();
        unreported.sort_unstable();
        anyhow::ensure!(
            unlisted.is_empty() && unreported.is_empty(),
            "the {prefix} vocabulary and Probe.swift disagree; \
             reported but not listed: {unlisted:?}; listed but never reported: {unreported:?}"
        );
        Ok(())
    }

    /// `(family, generation, tier)`, A line before M. Two tier scales share the
    /// last slot but never one generation: `A12` < `A12X` < `A12Z`, base < `Pro`.
    fn chip_rank(chip: &str) -> anyhow::Result<(u8, u32, u8)> {
        let mut words = chip.split_whitespace();
        anyhow::ensure!(
            words.next() == Some("Apple"),
            "chip {chip:?} does not start with the vendor name"
        );
        let part = words
            .next()
            .with_context(|| format!("chip {chip:?} names no part"))?;
        let (family, rest) = if let Some(rest) = part.strip_prefix('A') {
            (0, rest)
        } else if let Some(rest) = part.strip_prefix('M') {
            (1, rest)
        } else {
            anyhow::bail!("chip {chip:?} is in neither the A nor the M line")
        };
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        let generation: u32 = digits
            .parse()
            .with_context(|| format!("chip {chip:?} carries no generation number"))?;
        let suffix = rest.trim_start_matches(|c: char| c.is_ascii_digit());
        let tier = match (suffix, words.next()) {
            ("", None | Some("Bionic")) => 0,
            ("", Some("Pro")) | ("X", Some("Bionic")) => 1,
            ("Z", Some("Bionic")) => 2,
            _ => anyhow::bail!("chip {chip:?} names an unrecognized tier"),
        };
        Ok((family, generation, tier))
    }

    /// Each device flag's chip rank. This joins `Probe.swift`'s two maps
    /// on the machine identifier.
    fn device_chip_ranks(
        source: &str,
        prefix: &str,
    ) -> anyhow::Result<HashMap<String, (u8, u32, u8)>> {
        let chips: HashMap<String, String> =
            swift_map(source, "chipModelMapping")?.into_iter().collect();
        swift_map(source, "modelNameMapping")?
            .into_iter()
            .filter(|(identifier, _)| identifier.starts_with(prefix))
            .try_fold(HashMap::new(), |mut ranks, (identifier, name)| {
                let chip = chips
                    .get(&identifier)
                    .with_context(|| format!("{identifier} reports no chip"))?;
                let rank = chip_rank(chip)?;
                let device = format!("device:{}", pipette_core::client::slugify(&name));
                if let Some(previous) = ranks.insert(device.clone(), rank) {
                    anyhow::ensure!(
                        previous == rank,
                        "{device} reports two different chips across its machine identifiers"
                    );
                }
                Ok(ranks)
            })
    }

    /// Derived from each device's reported chip, not a second hand-written order.
    #[rstest]
    #[case::iphones("iPhone", IPHONES)]
    #[case::ipads("iPad", IPADS)]
    fn vocabularies_run_oldest_chip_first(
        #[case] prefix: &str,
        #[case] vocabulary: &[&str],
    ) -> anyhow::Result<()> {
        let source = device_probe_source()?;
        let ranks = device_chip_ranks(&source, prefix)?;
        let ranked: Vec<(u8, u32, u8)> = vocabulary
            .iter()
            .map(|entry| {
                ranks
                    .get(*entry)
                    .copied()
                    .with_context(|| format!("{entry:?} names no device in Probe.swift"))
            })
            .collect::<anyhow::Result<_>>()?;
        anyhow::ensure!(
            ranked.is_sorted(),
            "the {prefix} vocabulary is not oldest-chip-first: {:?}",
            vocabulary.iter().zip(&ranked).collect::<Vec<_>>()
        );
        Ok(())
    }

    /// Cases are brand strings rather than flags, so this fails if the family
    /// drifts from what a Mac actually reports.
    #[rstest]
    #[case::m1_base("Apple M1")]
    #[case::m1_pro("Apple M1 Pro")]
    #[case::m1_max("Apple M1 Max")]
    #[case::m1_ultra("Apple M1 Ultra")]
    #[case::m2_base("Apple M2")]
    #[case::m2_pro("Apple M2 Pro")]
    #[case::m2_max("Apple M2 Max")]
    #[case::m2_ultra("Apple M2 Ultra")]
    #[case::m3_base("Apple M3")]
    #[case::m3_pro("Apple M3 Pro")]
    #[case::m3_max("Apple M3 Max")]
    #[case::m3_ultra("Apple M3 Ultra")]
    #[case::m4_base("Apple M4")]
    #[case::m4_pro("Apple M4 Pro")]
    #[case::m4_max("Apple M4 Max")]
    // Read off a Mac17,6 in the device fleet.
    #[case::m5_max("Apple M5 Max")]
    fn real_macs_are_covered_by_the_apple_silicon_family(#[case] brand_string: &str) {
        let flag = format!("chip:{}", pipette_core::client::slugify(brand_string));
        assert!(
            APPLE_SILICON.contains(&flag.as_str()),
            "{brand_string:?} reports {flag:?}, which no MLX-on-macOS job would match"
        );
    }

    #[test]
    fn apple_silicon_runs_generation_by_generation() -> anyhow::Result<()> {
        const TIERS: [&str; 4] = ["", "pro", "max", "ultra"];
        let ranked: Vec<(u32, usize)> = APPLE_SILICON
            .iter()
            .map(|entry| {
                let part = entry
                    .strip_prefix("chip:applem")
                    .with_context(|| format!("{entry:?} does not name an M-series chip"))?;
                let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
                let generation: u32 = digits
                    .parse()
                    .with_context(|| format!("{entry:?} carries no generation number"))?;
                let suffix = part.trim_start_matches(|c: char| c.is_ascii_digit());
                let tier = TIERS
                    .iter()
                    .position(|t| *t == suffix)
                    .with_context(|| format!("{entry:?} names an unrecognized tier"))?;
                Ok((generation, tier))
            })
            .collect::<anyhow::Result<_>>()?;
        anyhow::ensure!(
            ranked.is_sorted(),
            "APPLE_SILICON is out of generation/tier order: {ranked:?}"
        );
        Ok(())
    }

    #[test]
    fn an_apple_silicon_floor_cuts_on_generation_not_speed() -> anyhow::Result<()> {
        let group = resolve_group(&Group::AtLeast {
            list: APPLE_SILICON,
            floor: "chip:applem3",
        })?;
        let forms: Vec<&str> = group.iter().map(CapabilityFlag::as_ref).collect();
        assert!(
            !forms.contains(&"chip:applem1ultra") && !forms.contains(&"chip:applem2ultra"),
            "an earlier generation stays out whatever its tier: {forms:?}"
        );
        assert!(
            forms.contains(&"chip:applem3") && forms.contains(&"chip:applem3ultra"),
            "the floor's own generation is admitted in full: {forms:?}"
        );
        Ok(())
    }

    /// Every member has to be canonical, or the server rejects the job at
    /// ingestion — the flags are written into `any_of` verbatim.
    #[test]
    fn apple_silicon_members_are_canonical_and_unique() -> anyhow::Result<()> {
        let mut seen = HashSet::new();
        APPLE_SILICON.iter().try_for_each(|entry| {
            anyhow::ensure!(seen.insert(*entry), "duplicate vocabulary entry {entry:?}");
            CapabilityFlag::try_new((*entry).to_owned())
                .with_context(|| format!("{entry:?} is not a canonical capability flag"))?;
            Ok(())
        })
    }
}
