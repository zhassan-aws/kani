// Copyright Kani Contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! This module defines the structure for a Kani project.
//! The goal is to provide one project view independent on the build system (cargo / standalone
//! rustc) and its configuration (e.g.: linker type).

use crate::metadata::from_json;
use crate::session::KaniSession;
use crate::util::crate_name;
use anyhow::{Context, Result};
use kani_metadata::{
    artifact::convert_type, ArtifactType, ArtifactType::*, HarnessMetadata, KaniMetadata, UnstableFeature,
};
use std::env::current_dir;
use std::fs;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{debug, trace};

/// This structure represent the project information relevant for verification.
/// A `Project` contains information about all crates under verification, as well as all
/// artifacts relevant for verification.
///
/// For one specific harness, there should be up to one artifact of each type. I.e., artifacts of
/// the same type are linked as part of creating the project.
///
/// However, one artifact can be used for multiple harnesses. This will depend on the type of
/// artifact, but it should be transparent for the user of this object.
#[derive(Debug, Default)]
pub struct Project {
    /// Each target crate metadata.
    pub metadata: Vec<KaniMetadata>,
    /// The directory where all outputs should be directed to. This path represents the canonical
    /// version of outdir.
    pub outdir: PathBuf,
    /// The path to the input file the project was built from.
    /// Note that it will only be `Some(...)` if this was built from a standalone project.
    pub input: Option<PathBuf>,
    /// The collection of artifacts kept as part of this project.
    artifacts: Vec<Artifact>,
    /// Records the cargo metadata from the build, if there was any
    pub cargo_metadata: Option<cargo_metadata::Metadata>,
    /// For build `keep_going` mode, we collect the targets that we failed to compile.
    pub failed_targets: Option<Vec<String>>,
}

impl Project {
    /// Get all harnesses from a project. This will include all test and proof harnesses.
    /// We could create a `get_proof_harnesses` and a `get_tests_harnesses` later if we see the
    /// need to split them.
    pub fn get_all_harnesses(&self) -> Vec<&HarnessMetadata> {
        self.metadata
            .iter()
            .flat_map(|crate_metadata| {
                crate_metadata.proof_harnesses.iter().chain(crate_metadata.test_harnesses.iter())
            })
            .collect()
    }

    /// Return the matching artifact for the given harness.
    ///
    /// If the harness has information about the goto_file we can use that to find the exact file.
    /// For cases where there is no goto_file, we just assume that everything has been linked
    /// together. I.e.: There should only be one artifact of the given type.
    pub fn get_harness_artifact(
        &self,
        harness: &HarnessMetadata,
        typ: ArtifactType,
    ) -> Option<&Artifact> {
        let expected_path = harness
            .goto_file
            .as_ref()
            .and_then(|goto_file| convert_type(goto_file, SymTabGoto, typ).canonicalize().ok());
        trace!(?harness.goto_file, ?expected_path, ?typ, "get_harness_artifact");
        self.artifacts.iter().find(|artifact| {
            artifact.has_type(typ)
                && expected_path.as_ref().map_or(true, |goto_file| *goto_file == artifact.path)
        })
    }

    /// Try to build a new project from the build result metadata.
    ///
    /// This method will parse the metadata in order to gather all artifacts generated by the
    /// compiler.
    fn try_new(
        session: &KaniSession,
        outdir: PathBuf,
        input: Option<PathBuf>,
        metadata: Vec<KaniMetadata>,
        cargo_metadata: Option<cargo_metadata::Metadata>,
        failed_targets: Option<Vec<String>>,
    ) -> Result<Self> {
        // For each harness (test or proof) from each metadata, read the path for the goto
        // SymTabGoto file. Use that path to find all the other artifacts.
        let mut artifacts = vec![];
        for crate_metadata in &metadata {
            for harness_metadata in
                crate_metadata.test_harnesses.iter().chain(crate_metadata.proof_harnesses.iter())
            {
                let symtab_out = Artifact::try_new(
                    harness_metadata.goto_file.as_ref().expect("Expected a model file"),
                    SymTabGoto,
                )?;
                let goto_path = convert_type(&symtab_out.path, symtab_out.typ, Goto);

                // Link
                session.link_goto_binary(&[symtab_out.to_path_buf()], &goto_path)?;
                let goto = Artifact::try_new(&goto_path, Goto)?;

                // All other harness artifacts that may have been generated as part of the build.
                artifacts.extend(
                    [SymTab, TypeMap, VTableRestriction, PrettyNameMap].iter().filter_map(|typ| {
                        let artifact = Artifact::try_from(&symtab_out, *typ).ok()?;
                        Some(artifact)
                    }),
                );
                artifacts.push(symtab_out);
                artifacts.push(goto);
            }
        }

        Ok(Project { outdir, input, metadata, artifacts, cargo_metadata, failed_targets })
    }
}

/// Information about a build artifact.
#[derive(Debug, Eq, PartialEq, Clone, Hash)]
pub struct Artifact {
    /// The path for this artifact in the canonical form.
    path: PathBuf,
    /// The type of artifact.
    typ: ArtifactType,
}

impl AsRef<Path> for Artifact {
    fn as_ref(&self) -> &Path {
        self.path.as_ref()
    }
}

impl Deref for Artifact {
    type Target = Path;
    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

impl Artifact {
    /// Create a new artifact if the given path exists.
    pub fn try_new(path: &Path, typ: ArtifactType) -> Result<Self> {
        Ok(Artifact {
            path: path.canonicalize().context(format!("Failed to process {}", path.display()))?,
            typ,
        })
    }

    /// Check if this artifact has the given type.
    pub fn has_type(&self, typ: ArtifactType) -> bool {
        self.typ == typ
    }

    /// Try to derive an artifact based on a different artifact of a different type.
    /// For example:
    /// ```no_run
    /// let artifact = Artifact::try_new(&"/tmp/file.kani_metadata.json", Metadata).unwrap();
    /// let goto = Artifact::try_from(artifact, Goto); // Will try to create "/tmp/file.goto"
    /// ```
    pub fn try_from(artifact: &Artifact, typ: ArtifactType) -> Result<Self> {
        Self::try_new(&convert_type(&artifact.path, artifact.typ, typ), typ)
    }
}

/// Generate a project using `cargo`.
/// Accept a boolean to build as many targets as possible. The number of failures in that case can
/// be collected from the project.
pub fn cargo_project(session: &KaniSession, keep_going: bool) -> Result<Project> {
    let outputs = session.cargo_build(keep_going)?;
    let outdir = outputs.outdir.canonicalize()?;
    // For the MIR Linker we know there is only one metadata per crate. Use that in our favor.
    let metadata =
        outputs.metadata.iter().map(|md_file| from_json(md_file)).collect::<Result<Vec<_>>>()?;
    
    let metadata = if session.args.common_args.unstable_features.contains(UnstableFeature::Aeneas) {
        let llbc_files: Vec<PathBuf> = metadata.iter().flat_map(|artifact: &KaniMetadata| artifact.proof_harnesses.iter().map(|md| {
            let mut file = md.goto_file.as_ref().unwrap().clone();
            file.set_extension("llbc");
            file
        })).collect();
        for llbc_file in llbc_files {
            let mut cmd = Command::new("aeneas");
            cmd.arg("-backend");
            cmd.arg("lean");
            cmd.arg(llbc_file);
            session.run_terminal(cmd)?;
        }
        Vec::new()
    } else {
        metadata
    };
    Project::try_new(
        session,
        outdir,
        None,
        metadata,
        Some(outputs.cargo_metadata),
        outputs.failed_targets,
    )
}

/// Generate a project directly using `kani-compiler` on a single crate.
pub fn standalone_project(
    input: &Path,
    crate_name: Option<String>,
    session: &KaniSession,
) -> Result<Project> {
    StandaloneProjectBuilder::try_new(input, crate_name, session)?.build()
}

/// Builder for a standalone project.
struct StandaloneProjectBuilder<'a> {
    /// The directory where all outputs should be directed to.
    outdir: PathBuf,
    /// The metadata file for the target crate.
    metadata: Artifact,
    /// The input file.
    input: PathBuf,
    /// The crate name.
    crate_name: String,
    /// The Kani session.
    session: &'a KaniSession,
}

impl<'a> StandaloneProjectBuilder<'a> {
    /// Create a `StandaloneProjectBuilder` from the given input and session.
    /// This will perform a few validations before the build.
    fn try_new(input: &Path, krate_name: Option<String>, session: &'a KaniSession) -> Result<Self> {
        // Ensure the directory exist and it's in its canonical form.
        let outdir = if let Some(target_dir) = &session.args.target_dir {
            std::fs::create_dir_all(target_dir)?; // This is a no-op if directory exists.
            target_dir.canonicalize()?
        } else {
            input.canonicalize().unwrap().parent().unwrap().to_path_buf()
        };
        let crate_name = if let Some(name) = krate_name { name } else { crate_name(&input) };
        let metadata = standalone_artifact(&outdir, &crate_name, Metadata);
        Ok(StandaloneProjectBuilder {
            outdir,
            metadata,
            input: input.to_path_buf(),
            crate_name,
            session,
        })
    }

    /// Build a project by compiling `self.input` file.
    fn build(self) -> Result<Project> {
        // Register artifacts that may be generated by the compiler / linker for future deletion.
        let rlib_path = self.rlib_name();
        self.session.record_temporary_file(&rlib_path);
        self.session.record_temporary_file(&self.metadata.path);

        // Build and link the artifacts.
        debug!(krate=?self.crate_name, input=?self.input, ?rlib_path, "build compile");
        self.session.compile_single_rust_file(&self.input, &self.crate_name, &self.outdir)?;

        let metadata = from_json(&self.metadata)?;

        // Create the project with the artifacts built by the compiler.
        let result = Project::try_new(
            self.session,
            self.outdir,
            Some(self.input),
            vec![metadata],
            None,
            None,
        );
        if let Ok(project) = &result {
            self.session.record_temporary_files(&project.artifacts);
        }
        result
    }

    /// Build the rlib name from the crate name.
    /// This is only used by 'kani', never 'cargo-kani', so we hopefully don't have too many corner
    /// cases to deal with.
    fn rlib_name(&self) -> PathBuf {
        let path = &self.outdir.join(self.input.file_name().unwrap());
        let basedir = path.parent().unwrap_or(Path::new("."));
        let rlib_name = format!("lib{}.rlib", self.crate_name);

        basedir.join(rlib_name)
    }
}

/// Generate the expected path of a standalone artifact of the given type.
// Note: `out_dir` is already on canonical form, so no need to invoke `try_new()`.
fn standalone_artifact(out_dir: &Path, crate_name: &String, typ: ArtifactType) -> Artifact {
    let mut path = out_dir.join(crate_name);
    let _ = path.set_extension(typ);
    Artifact { path, typ }
}

/// Verify the custom version of the standard library in the given path.
///
/// Note that we assume that `std_path` points to a directory named "library".
/// This should be checked as part of the argument validation.
pub(crate) fn std_project(std_path: &Path, session: &KaniSession) -> Result<Project> {
    // Create output directory
    let outdir = if let Some(target_dir) = &session.args.target_dir {
        target_dir.clone()
    } else {
        current_dir()?.join("target")
    };
    fs::create_dir_all(&outdir)?; // This is a no-op if directory exists.
    let outdir = outdir.canonicalize()?;

    // Create dummy crate needed to build using `cargo -Z build-std`
    let dummy_crate = outdir.join("kani_verify_std");
    if dummy_crate.exists() {
        fs::remove_dir_all(&dummy_crate)?;
    }
    session.cargo_init_lib(&dummy_crate)?;

    // Build cargo project for dummy crate.
    let std_path = std_path.canonicalize()?;
    let outputs = session.cargo_build_std(std_path.parent().unwrap(), &dummy_crate)?;

    // Get the metadata and return a Kani project.
    let metadata = outputs.iter().map(|md_file| from_json(md_file)).collect::<Result<Vec<_>>>()?;
    Project::try_new(session, outdir, None, metadata, None, None)
}
