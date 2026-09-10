//! Attribution: which project does a resource belong to, and how sure are we?
//!
//! Provenance in Docker is asymmetric, and the whole design follows from that:
//!
//! * **Containers** carry an absolute host path (`com.docker.compose.project.working_dir`,
//!   `com.ddev.approot`, `devcontainer.local_folder`).
//! * **Volumes carry no path at all.** An exhaustive scan of 259 volumes on the
//!   reference machine found no host path in any label. ddev volumes carry no
//!   labels whatsoever.
//! * **Images** carry a project name, never a path.
//!
//! So containers are harvested first into a [`Catalog`] mapping project name to
//! absolute path, and everything else is resolved against that. Once a container
//! is removed the mapping is gone forever — which is why the index exists, and
//! why nothing may prune containers before recording their edges.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::model::{
    Claim, Confidence, Evidence, EvidenceSource, Liveness, ProjectId, ProviderKind, ResourceKind,
    ResourceSummary,
};

pub const COMPOSE_PROJECT: &str = "com.docker.compose.project";
pub const COMPOSE_WORKING_DIR: &str = "com.docker.compose.project.working_dir";
pub const COMPOSE_CONFIG_FILES: &str = "com.docker.compose.project.config_files";
pub const DDEV_APPROOT: &str = "com.ddev.approot";
pub const DDEV_SITE_NAME: &str = "com.ddev.site-name";
pub const DEVCONTAINER_FOLDER: &str = "devcontainer.local_folder";
pub const SUPABASE_PROJECT: &str = "com.supabase.cli.project";

/// A project we know about, and where it lives.
#[derive(Clone, Debug)]
pub struct KnownProject {
    pub name: String,
    pub provider: ProviderKind,
    pub root: Option<PathBuf>,
    /// Resources this project's config declares, whether or not they exist.
    /// A declared volume is *referenced* even with no containers running — this
    /// is what stops `cedar-wordpress_mysql` being called an orphan.
    pub declared: BTreeSet<String>,
}

impl KnownProject {
    pub fn id(&self) -> ProjectId {
        match &self.root {
            Some(p) => ProjectId(p.to_string_lossy().into_owned()),
            None => ProjectId(format!("{}:{}", self.provider.as_str(), self.name)),
        }
    }
}

/// Everything discovered about projects during one scan, plus whatever the
/// filesystem can confirm.
#[derive(Debug, Default)]
pub struct Catalog {
    by_name: BTreeMap<String, KnownProject>,
    /// Project names longest-first, for prefix matching.
    names_by_length: Vec<String>,
}

impl Catalog {
    pub fn get(&self, name: &str) -> Option<&KnownProject> {
        self.by_name.get(name)
    }

    pub fn projects(&self) -> impl Iterator<Item = &KnownProject> {
        self.by_name.values()
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    fn insert(&mut self, p: KnownProject) {
        // A claim with a real path always beats one without.
        match self.by_name.get(&p.name) {
            Some(existing) if existing.root.is_some() && p.root.is_none() => return,
            _ => {}
        }
        self.by_name.insert(p.name.clone(), p);
    }

    fn reindex(&mut self) {
        let mut names: Vec<String> = self.by_name.keys().cloned().collect();
        names.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
        self.names_by_length = names;
    }

    /// Harvest projects from containers — the only resources that carry paths.
    ///
    /// Deliberately reads every container regardless of state: an exited
    /// container's labels are just as authoritative, and are often the last
    /// surviving record of where a project lived.
    pub fn harvest(containers: &[ResourceSummary]) -> Self {
        let mut cat = Catalog::default();

        for c in containers {
            // ddev first: `approot` is the project root, whereas ddev's compose
            // `working_dir` points at the `.ddev` subdirectory.
            if let (Some(site), Some(approot)) = (
                c.label_nonempty(DDEV_SITE_NAME),
                c.label_nonempty(DDEV_APPROOT),
            ) {
                cat.insert(KnownProject {
                    name: site.to_string(),
                    provider: ProviderKind::Ddev,
                    root: Some(PathBuf::from(approot)),
                    declared: BTreeSet::new(),
                });
            }

            if let Some(folder) = c.label_nonempty(DEVCONTAINER_FOLDER) {
                let name = Path::new(folder)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| folder.to_string());
                cat.insert(KnownProject {
                    name,
                    provider: ProviderKind::Devcontainer,
                    root: Some(PathBuf::from(folder)),
                    declared: BTreeSet::new(),
                });
            }

            if let Some(project) = c.label_nonempty(COMPOSE_PROJECT) {
                // `working_dir` may be present but EMPTY — the supabase CLI
                // stamps it that way. `label_nonempty` is what makes that safe.
                let root = c.label_nonempty(COMPOSE_WORKING_DIR).map(PathBuf::from);
                let provider = if c.label_nonempty(SUPABASE_PROJECT).is_some() {
                    ProviderKind::Supabase
                } else {
                    ProviderKind::Compose
                };
                cat.insert(KnownProject {
                    name: project.to_string(),
                    provider,
                    root,
                    declared: BTreeSet::new(),
                });
            }
        }

        cat.reindex();
        cat
    }

    /// Enrich the catalog from disk.
    ///
    /// Two jobs, and the second one matters more than it looks:
    ///
    /// 1. Add projects that have no containers at all.
    /// 2. **Attach declared volumes to projects already harvested from
    ///    container labels.** Nearly every project has containers, so skipping
    ///    the ones already known would mean the declaration cross-check almost
    ///    never runs — and that cross-check is what separates `Proven` from
    ///    `Strong`, and what stops a declared-but-idle volume being called an
    ///    orphan.
    pub fn add_disk_projects(&mut self, roots: &[PathBuf]) {
        for root in roots {
            let Ok(entries) = std::fs::read_dir(root) else {
                // A configured root we cannot read is a blocking condition for
                // everything under it, never a licence to orphan it. The caller
                // decides what to do; we simply add nothing.
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let Some(name) = path.file_name().map(|s| s.to_string_lossy().into_owned()) else {
                    continue;
                };

                let declared = declared_volumes(&path);

                if let Some(existing) = self.by_name.get_mut(&name) {
                    // Already known from a container label. Keep the
                    // authoritative path, but take the declarations.
                    if existing.root.is_none() {
                        existing.root = Some(path.clone());
                    }
                    if !declared.is_empty() {
                        existing.declared.extend(declared);
                    }
                    continue;
                }

                let provider = if path.join(".ddev").is_dir() {
                    ProviderKind::Ddev
                } else if path.join(".devcontainer").exists() {
                    ProviderKind::Devcontainer
                } else if has_compose_file(&path) {
                    ProviderKind::Compose
                } else {
                    continue;
                };
                self.insert(KnownProject {
                    name,
                    provider,
                    root: Some(path.clone()),
                    declared,
                });
            }
        }
        self.reindex();
    }

    /// Longest-prefix match of a resource name against known project names.
    ///
    /// This replaces regex suffix-stripping, which produced false orphans:
    /// stripping `_mysql` from `cedar-wordpress_mysql` yields `cedar`, and a
    /// naive matcher then reports an orphan while `cedar-wordpress` sits right
    /// there on disk.
    ///
    /// Only a match on a separator boundary counts, and the longest wins.
    pub fn longest_prefix(&self, resource_name: &str) -> Option<&KnownProject> {
        for name in &self.names_by_length {
            if resource_name == name.as_str() {
                return self.by_name.get(name);
            }
            if let Some(rest) = resource_name.strip_prefix(name.as_str()) {
                if rest.starts_with('_') || rest.starts_with('-') {
                    return self.by_name.get(name);
                }
            }
        }
        None
    }
}

fn has_compose_file(dir: &Path) -> bool {
    [
        "docker-compose.yml",
        "docker-compose.yaml",
        "compose.yml",
        "compose.yaml",
    ]
    .iter()
    .any(|f| dir.join(f).is_file())
}

/// Volume names a project's compose file declares.
///
/// A deliberately shallow parse: we want the keys under a top-level `volumes:`
/// block, not a full YAML implementation. Being wrong here is safe in one
/// direction only — a missed declaration can produce a false orphan, so when in
/// doubt we include rather than exclude.
fn declared_volumes(dir: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for f in [
        "docker-compose.yml",
        "docker-compose.yaml",
        "compose.yml",
        "compose.yaml",
    ] {
        let Ok(text) = std::fs::read_to_string(dir.join(f)) else {
            continue;
        };
        let mut in_volumes = false;
        for line in text.lines() {
            let trimmed = line.trim_end();
            if trimmed.trim_start().starts_with('#') || trimmed.trim().is_empty() {
                continue;
            }
            let indent = trimmed.len() - trimmed.trim_start().len();
            if indent == 0 {
                in_volumes = trimmed.trim_end_matches(':').trim() == "volumes";
                continue;
            }
            if in_volumes && indent <= 2 {
                if let Some(name) = trimmed.trim().strip_suffix(':') {
                    if !name.is_empty() && !name.starts_with('-') {
                        out.insert(name.to_string());
                    }
                }
            }
        }
    }
    out
}

/// Does this project directory currently exist?
///
/// **Absence of a directory is never sufficient on its own to call something an
/// orphan.** An unmounted external disk or a not-yet-cloned repo looks exactly
/// like a deleted project from here, and treating them alike would mass-orphan
/// an entire root in a single run. The caller must additionally require that
/// the parent root is present and readable, and that the index has seen the
/// project absent across several scans.
pub fn liveness_of(root: Option<&PathBuf>) -> Liveness {
    match root {
        None => Liveness::Unverifiable {
            reason: "no path is recorded for this project".to_string(),
        },
        Some(p) if p.is_dir() => Liveness::Present,
        Some(p) => {
            // If the parent is missing too, the whole root may be unmounted —
            // that is unverifiable, not absent.
            match p.parent() {
                Some(parent) if parent.is_dir() => Liveness::Absent {
                    since_unix: None,
                    scans: 1,
                },
                Some(parent) => Liveness::Unverifiable {
                    reason: format!("{} is not readable", parent.display()),
                },
                None => Liveness::Unverifiable {
                    reason: "path has no parent".to_string(),
                },
            }
        }
    }
}

/// Every claim any provider makes about one resource.
///
/// All providers run and every claim is kept, because a competing claim is
/// itself evidence: a `Weak` claim from a project that still exists vetoes an
/// orphan verdict from a `Strong` claim on a project that does not.
pub fn claims_for(res: &ResourceSummary, cat: &Catalog) -> Vec<Claim> {
    let mut out = Vec::new();

    // --- devcontainer: an absolute path, directly on the resource ---------
    if let Some(folder) = res.label_nonempty(DEVCONTAINER_FOLDER) {
        let root = PathBuf::from(folder);
        let name = root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| folder.to_string());
        out.push(Claim {
            project: ProjectId(folder.to_string()),
            project_name: name,
            provider: ProviderKind::Devcontainer,
            confidence: Confidence::Strong,
            liveness: liveness_of(Some(&root)),
            root: Some(root),
            evidence: vec![Evidence::new(
                EvidenceSource::Label,
                format!("{DEVCONTAINER_FOLDER} = {folder}"),
            )],
        });
    }

    // --- ddev: approot is the project root -------------------------------
    if let Some(approot) = res.label_nonempty(DDEV_APPROOT) {
        let root = PathBuf::from(approot);
        let name = res
            .label_nonempty(DDEV_SITE_NAME)
            .map(str::to_string)
            .unwrap_or_else(|| {
                root.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
        out.push(Claim {
            project: ProjectId(approot.to_string()),
            project_name: name,
            provider: ProviderKind::Ddev,
            confidence: Confidence::Strong,
            liveness: liveness_of(Some(&root)),
            root: Some(root),
            evidence: vec![Evidence::new(
                EvidenceSource::Label,
                format!("{DDEV_APPROOT} = {approot}"),
            )],
        });
    }

    // --- compose / supabase: a project NAME, resolved via the catalog -----
    if let Some(project) = res.label_nonempty(COMPOSE_PROJECT) {
        let supabase = res.label_nonempty(SUPABASE_PROJECT).is_some();
        let provider = if supabase {
            ProviderKind::Supabase
        } else {
            ProviderKind::Compose
        };

        let direct_root = res.label_nonempty(COMPOSE_WORKING_DIR).map(PathBuf::from);
        let known = cat.get(project);
        let root = direct_root
            .clone()
            .or_else(|| known.and_then(|k| k.root.clone()));

        let mut evidence = vec![Evidence::new(
            EvidenceSource::Label,
            format!("{COMPOSE_PROJECT} = {project}"),
        )];

        // Proven needs BOTH an existing path AND a declaration naming this
        // resource. Strong is an authoritative label whose path exists.
        let declared = known
            .map(|k| {
                k.declared
                    .iter()
                    .any(|d| declared_matches(d, project, &res.name))
            })
            .unwrap_or(false);

        let confidence = match (&root, direct_root.is_some(), declared) {
            (Some(p), _, true) if p.is_dir() => {
                evidence.push(Evidence::new(
                    EvidenceSource::Fs,
                    format!("declared by the compose file in {}", p.display()),
                ));
                Confidence::Proven
            }
            (Some(p), true, _) if p.is_dir() => {
                evidence.push(Evidence::new(
                    EvidenceSource::Label,
                    format!("{COMPOSE_WORKING_DIR} = {}", p.display()),
                ));
                Confidence::Strong
            }
            (Some(p), false, _) if p.is_dir() => {
                evidence.push(Evidence::new(
                    EvidenceSource::Index,
                    format!("project path {} resolved from a container", p.display()),
                ));
                Confidence::Strong
            }
            // A label naming a path that is NOT on disk is still authoritative
            // about ownership — it is the liveness that is in question, and
            // that is decided later with the index, never here.
            (Some(p), _, _) => {
                evidence.push(Evidence::new(
                    EvidenceSource::Fs,
                    format!("{} is not present", p.display()),
                ));
                Confidence::Strong
            }
            (None, _, _) => Confidence::Weak,
        };

        out.push(Claim {
            project: ProjectId(
                root.as_ref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| format!("{}:{project}", provider.as_str())),
            ),
            project_name: project.to_string(),
            provider,
            confidence,
            liveness: liveness_of(root.as_ref()),
            root,
            evidence,
        });
    }

    // --- heuristic: name shape only, and capped at Guess ------------------
    //
    // ddev volumes carry NO labels at all, so `<site>-mariadb` and
    // `<site>_project_mutagen` are the only handle we have on them. That is a
    // guess, and a guess may raise a question but may never justify a deletion.
    if out.is_empty() && res.kind == ResourceKind::Volume && !res.is_anonymous_volume() {
        if let Some(k) = cat.longest_prefix(&res.name) {
            out.push(Claim {
                project: k.id(),
                project_name: k.name.clone(),
                provider: ProviderKind::Heuristic,
                confidence: Confidence::Guess,
                liveness: liveness_of(k.root.as_ref()),
                root: k.root.clone(),
                evidence: vec![Evidence::new(
                    EvidenceSource::Heuristic,
                    format!("name starts with the known project \"{}\"", k.name),
                )],
            });
        }
    }

    out
}

/// Is this volume declared by any project that currently exists on disk?
///
/// This is the edge that stops `cedar-wordpress_mysql` being called an orphan
/// when nothing happens to be running: a declaration is a reference.
pub fn declared_by_live_project(volume_name: &str, cat: &Catalog) -> Option<String> {
    for p in cat.projects() {
        let live = matches!(liveness_of(p.root.as_ref()), Liveness::Present);
        if !live {
            continue;
        }
        if p.declared
            .iter()
            .any(|d| declared_matches(d, &p.name, volume_name))
        {
            return Some(p.name.clone());
        }
    }
    None
}

/// Does a declared volume name correspond to this actual volume?
///
/// Compose prefixes declared volume names with the project name, so a
/// declaration of `mysql` in project `oak` becomes the volume `oak_mysql`.
fn declared_matches(declared: &str, project: &str, volume_name: &str) -> bool {
    volume_name == declared || volume_name == format!("{project}_{declared}")
}

/// Pick the winning claim.
///
/// Highest confidence, then provider priority, then longest matched path.
pub fn best_claim(claims: &[Claim]) -> Option<&Claim> {
    claims.iter().max_by(|a, b| {
        a.confidence
            .cmp(&b.confidence)
            .then(a.provider.priority().cmp(&b.provider.priority()))
            .then_with(|| {
                let al = a.root.as_ref().map(|p| p.as_os_str().len()).unwrap_or(0);
                let bl = b.root.as_ref().map(|p| p.as_os_str().len()).unwrap_or(0);
                al.cmp(&bl)
            })
    })
}

/// How many consecutive scans a project must be missing before its resources
/// can be called orphaned.
///
/// One observation is never enough. An unmounted external disk, a repo not yet
/// cloned on this laptop, a network share that was slow to appear — all look
/// exactly like a deleted project for the length of a single scan. Two
/// consecutive misses rules out the one-off without making the user wait a week.
pub const MIN_ABSENT_SCANS: u32 = 2;

/// Would calling this resource an orphan be defensible?
///
/// Requires a `Strong` or better claim naming an absent project, **and** no
/// competing claim of any strength pointing at a project that is still present.
/// `Unverifiable` liveness never qualifies.
pub fn is_orphan_candidate(claims: &[Claim]) -> bool {
    let any_present = claims.iter().any(|c| c.liveness == Liveness::Present);
    if any_present {
        return false;
    }
    claims.iter().any(|c| {
        c.confidence >= Confidence::Strong
            && matches!(c.liveness, Liveness::Absent { scans, .. } if scans >= MIN_ABSENT_SCANS)
    })
}

/// Absent, but not yet for long enough to act on.
pub fn is_orphan_pending(claims: &[Claim]) -> Option<u32> {
    if claims.iter().any(|c| c.liveness == Liveness::Present) {
        return None;
    }
    claims
        .iter()
        .filter(|c| c.confidence >= Confidence::Strong)
        .filter_map(|c| match c.liveness {
            Liveness::Absent { scans, .. } if scans < MIN_ABSENT_SCANS => Some(scans),
            _ => None,
        })
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ResourceKind;

    fn container(name: &str, labels: &[(&str, &str)]) -> ResourceSummary {
        let mut r = ResourceSummary::new(ResourceKind::Container, name, name);
        for (k, v) in labels {
            r.labels.insert((*k).to_string(), (*v).to_string());
        }
        r
    }

    fn volume(name: &str, labels: &[(&str, &str)]) -> ResourceSummary {
        let mut r = ResourceSummary::new(ResourceKind::Volume, name, name);
        for (k, v) in labels {
            r.labels.insert((*k).to_string(), (*v).to_string());
        }
        r
    }

    #[test]
    fn harvest_reads_paths_only_from_containers() {
        let cs = vec![
            container(
                "oak-wordpress-1",
                &[
                    (COMPOSE_PROJECT, "oak"),
                    (COMPOSE_WORKING_DIR, "/Users/x/Repos/oak"),
                ],
            ),
            container(
                "ddev-cedar-v2-web-web",
                &[
                    (DDEV_SITE_NAME, "cedar-v2-web"),
                    (DDEV_APPROOT, "/Users/x/Repos/cedar-wordpress"),
                    (COMPOSE_PROJECT, "ddev-cedar-v2-web"),
                    (COMPOSE_WORKING_DIR, "/Users/x/Repos/cedar-wordpress/.ddev"),
                ],
            ),
        ];
        let cat = Catalog::harvest(&cs);

        assert_eq!(
            cat.get("oak").unwrap().root.as_ref().unwrap(),
            &PathBuf::from("/Users/x/Repos/oak")
        );
        // ddev's approot is the project root; compose's working_dir points at
        // the .ddev subdirectory, which is not what we want.
        assert_eq!(
            cat.get("cedar-v2-web").unwrap().root.as_ref().unwrap(),
            &PathBuf::from("/Users/x/Repos/cedar-wordpress")
        );
    }

    #[test]
    fn supabase_empty_working_dir_does_not_become_a_root() {
        // The supabase CLI stamps working_dir with "". Treating a present-but-
        // empty label as a path would root the project at "".
        let cs = vec![container(
            "supabase_db_knife-template-maker",
            &[
                (COMPOSE_PROJECT, "knife-template-maker"),
                (COMPOSE_WORKING_DIR, ""),
                (SUPABASE_PROJECT, "knife-template-maker"),
            ],
        )];
        let cat = Catalog::harvest(&cs);
        let p = cat.get("knife-template-maker").unwrap();
        assert!(p.root.is_none());
        assert_eq!(p.provider, ProviderKind::Supabase);
    }

    #[test]
    fn longest_prefix_beats_the_regex_suffix_bug() {
        // The original regex approach stripped `_mysql` and then `wordpress`,
        // leaving `cedar`, which matched nothing and produced a false orphan.
        let cs = vec![
            container(
                "cedar",
                &[
                    (COMPOSE_PROJECT, "cedar"),
                    (COMPOSE_WORKING_DIR, "/r/cedar"),
                ],
            ),
            container(
                "cedar-wordpress",
                &[
                    (COMPOSE_PROJECT, "cedar-wordpress"),
                    (COMPOSE_WORKING_DIR, "/r/cedar-wordpress"),
                ],
            ),
        ];
        let cat = Catalog::harvest(&cs);

        let hit = cat.longest_prefix("cedar-wordpress_mysql").unwrap();
        assert_eq!(
            hit.name, "cedar-wordpress",
            "the longer project must win over the shorter prefix"
        );
    }

    #[test]
    fn prefix_match_requires_a_separator_boundary() {
        let cs = vec![container(
            "web",
            &[(COMPOSE_PROJECT, "web"), (COMPOSE_WORKING_DIR, "/r/web")],
        )];
        let cat = Catalog::harvest(&cs);

        assert!(cat.longest_prefix("web_data").is_some());
        assert!(cat.longest_prefix("web-cache").is_some());
        // `website_data` must NOT be attributed to project `web`.
        assert!(cat.longest_prefix("website_data").is_none());
    }

    #[test]
    fn heuristic_claims_are_capped_at_guess() {
        let cs = vec![container(
            "saffronfields",
            &[
                (COMPOSE_PROJECT, "saffronfields"),
                (COMPOSE_WORKING_DIR, "/r/saffronfields"),
            ],
        )];
        let cat = Catalog::harvest(&cs);

        // A ddev volume carries no labels at all — name shape is all we have.
        let v = volume("saffronfields-mariadb", &[]);
        let claims = claims_for(&v, &cat);
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].confidence, Confidence::Guess);
        assert_eq!(claims[0].provider, ProviderKind::Heuristic);
    }

    #[test]
    fn anonymous_volumes_get_no_heuristic_claim() {
        let cat = Catalog::harvest(&[]);
        let v = volume(&"a".repeat(64), &[]);
        assert!(
            claims_for(&v, &cat).is_empty(),
            "a 64-hex name carries no provenance and must not be guessed at"
        );
    }

    #[test]
    fn one_missing_observation_is_never_enough_for_an_orphan() {
        // The F3 mitigation. An unplugged disk looks exactly like a deleted
        // project for the length of one scan.
        let one = Claim {
            project: ProjectId("/gone".into()),
            project_name: "gone".into(),
            provider: ProviderKind::Compose,
            confidence: Confidence::Strong,
            root: Some(PathBuf::from("/gone")),
            liveness: Liveness::Absent {
                since_unix: None,
                scans: 1,
            },
            evidence: vec![],
        };
        assert!(!is_orphan_candidate(std::slice::from_ref(&one)));
        assert_eq!(is_orphan_pending(&[one]), Some(1));

        let confirmed = Claim {
            liveness: Liveness::Absent {
                since_unix: None,
                scans: MIN_ABSENT_SCANS,
            },
            ..Claim {
                project: ProjectId("/gone".into()),
                project_name: "gone".into(),
                provider: ProviderKind::Compose,
                confidence: Confidence::Strong,
                root: Some(PathBuf::from("/gone")),
                liveness: Liveness::Present,
                evidence: vec![],
            }
        };
        assert!(is_orphan_candidate(std::slice::from_ref(&confirmed)));
        assert_eq!(is_orphan_pending(&[confirmed]), None);
    }

    #[test]
    fn a_present_project_vetoes_an_orphan_verdict() {
        let absent = Claim {
            project: ProjectId("/gone".into()),
            project_name: "gone".into(),
            provider: ProviderKind::Compose,
            confidence: Confidence::Strong,
            root: Some(PathBuf::from("/gone")),
            liveness: Liveness::Absent {
                since_unix: None,
                scans: 9,
            },
            evidence: vec![],
        };
        let present = Claim {
            liveness: Liveness::Present,
            confidence: Confidence::Weak,
            ..absent.clone()
        };

        assert!(is_orphan_candidate(std::slice::from_ref(&absent)));
        assert!(
            !is_orphan_candidate(&[absent, present]),
            "any live claim, however weak, must veto an orphan verdict"
        );
    }

    #[test]
    fn unverifiable_liveness_never_yields_an_orphan() {
        // An unmounted external disk must not mass-orphan every project on it.
        let c = Claim {
            project: ProjectId("/Volumes/Work/p".into()),
            project_name: "p".into(),
            provider: ProviderKind::Compose,
            confidence: Confidence::Proven,
            root: Some(PathBuf::from("/Volumes/Work/p")),
            liveness: Liveness::Unverifiable {
                reason: "/Volumes/Work is not readable".into(),
            },
            evidence: vec![],
        };
        assert!(!is_orphan_candidate(&[c]));
    }

    #[test]
    fn best_claim_prefers_confidence_then_provider() {
        let base = Claim {
            project: ProjectId("/p".into()),
            project_name: "p".into(),
            provider: ProviderKind::Heuristic,
            confidence: Confidence::Guess,
            root: None,
            liveness: Liveness::Present,
            evidence: vec![],
        };
        let strong = Claim {
            provider: ProviderKind::Compose,
            confidence: Confidence::Strong,
            ..base.clone()
        };
        let set = [base.clone(), strong.clone()];
        let picked = best_claim(&set).unwrap();
        assert_eq!(picked.confidence, Confidence::Strong);

        // Equal confidence: compose outranks the heuristic.
        let tie = Claim {
            confidence: Confidence::Strong,
            ..base.clone()
        };
        let set = [tie, strong];
        let picked = best_claim(&set).unwrap();
        assert_eq!(picked.provider, ProviderKind::Compose);
    }

    #[test]
    fn declared_volume_names_are_project_prefixed() {
        assert!(declared_matches("mysql", "oak", "oak_mysql"));
        assert!(declared_matches("oak_mysql", "oak", "oak_mysql"));
        assert!(!declared_matches("mysql", "oak", "fen_mysql"));
    }
}
