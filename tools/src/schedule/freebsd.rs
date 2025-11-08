use crate::args::PkgsSync;
use crate::schedule::{fetch_url_or_path, Pkg};
use rebuilderd_common::api::v1::{BinaryPackageReport, PackageReport, SourcePackageReport};
use rebuilderd_common::errors::*;
use rebuilderd_common::http;
use serde_json;
use std::io::{BufRead, BufReader, Read};
use tar;
use zstd::stream::read::Decoder as ZstdDecoder;

#[derive(Debug, serde::Deserialize)]
struct FreeBSDPkg {
    name: String,
    origin: String,
    version: String,
    arch: String,
}

impl Pkg for FreeBSDPkg {
    fn pkg_name(&self) -> &str {
        &self.name
    }

    fn by_maintainer(&self, _maintainers: &[String]) -> bool {
        // packagesite.yaml does not contain maintainer info.
        false
    }
}

pub async fn sync(http: &http::Client, sync: &PkgsSync) -> Result<Vec<PackageReport>> {
    let mut reports = Vec::new();

    // FreeBSD sync only tracks "latest" branch, not "quarterly"
    let component = "latest";

    for release in &sync.releases {
        for arch in &sync.architectures {
            let url = format!(
                "{}/FreeBSD:{}:{}/{}/packagesite.pkg",
                sync.source, release, arch, component
            );

            let bytes = match fetch_url_or_path(http, &url).await {
                Ok(b) => {
                    info!("Successfully fetched {}", url);
                    b
                }
                Err(e) => {
                    warn!("Failed to fetch {}: {}. Skipping this release/arch.", url, e);
                    continue;
                }
            };

            let tar = ZstdDecoder::new(&bytes[..])?;
            let mut archive = tar::Archive::new(tar);

            let mut packagesite_json = Vec::new();
            let mut found_packagesite = false;
            for entry in archive.entries()? {
                let mut entry = entry?;
                if entry.path()?.to_str().unwrap_or_default() == "packagesite.yaml" {
                    entry.read_to_end(&mut packagesite_json)?;
                    found_packagesite = true;
                    break;
                }
            }

            if !found_packagesite {
                warn!("Could not find packagesite.yaml in {} for {}/{}. Skipping.", url, release, arch);
                continue;
            }

            let reader = BufReader::new(&packagesite_json[..]);
            let mut source_packages = Vec::new();

            for line in reader.lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }

                let pkg: FreeBSDPkg = match serde_json::from_str(&line) {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("Failed to parse package line, skipping: {}. Error: {}", line, e);
                        continue;
                    }
                };

                if !pkg.matches(sync) {
                    continue;
                }

                let pkg_url = format!(
                    "{}/FreeBSD:{}:{}/{}/All/{}-{}.pkg",
                    sync.source, release, arch, component, pkg.name, pkg.version
                );

                let source_report = SourcePackageReport {
                    name: pkg.origin.clone(),
                    version: pkg.version.clone(),
                    url: pkg_url.clone(), // Point to the .pkg file for download
                    artifacts: vec![BinaryPackageReport {
                        name: pkg.name.clone(),
                        version: pkg.version.clone(),
                        architecture: pkg.arch.clone(),
                        url: pkg_url,
                    }],
                };
                source_packages.push(source_report);
            }

            reports.push(PackageReport {
                distribution: "freebsd".to_string(),
                release: Some(release.clone()),
                component: Some(component.to_string()),
                architecture: arch.clone(),
                packages: source_packages,
            });
        }
    }

    Ok(reports)
}