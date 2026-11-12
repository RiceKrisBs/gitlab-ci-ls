use std::{
    collections::HashMap,
    fs::{self, File},
    io::Write,
    path::{self, Path},
    process::Command,
    time::Duration,
};

use super::{
    fs_utils::{self, FSUtils},
    parser_utils::{self, ComponentInfo, ParserUtils},
    GitlabElement, GitlabFile, ProjectFile, DEFAULT_BRANCH_SUBFOLDER,
};
use log::{debug, error, info};
use reqwest::{blocking::Client, header::IF_NONE_MATCH, StatusCode, Url};

/// How long a cached project clone is trusted before startup pulls it again.
const PULL_INTERVAL: Duration = Duration::from_hours(2);

pub trait Git {
    fn clone_repo(&self, repo_dest: &str, remote_tag: Option<&str>, remote_pkg: &str);
    fn fetch_remote_repository(
        &self,
        remote_pkg: &str,
        remote_tag: Option<&str>,
        remote_files: ProjectFile,
    ) -> anyhow::Result<Vec<GitlabFile>>;
    fn fetch_remote(&self, url: Url) -> anyhow::Result<GitlabFile>;
    fn fetch_remote_component(
        &self,
        component_info: ComponentInfo,
    ) -> anyhow::Result<GitlabElement>;
    fn get_project_remote_uris(&self) -> Vec<String>;
}

#[allow(clippy::module_name_repetitions)]
pub struct GitImpl {
    package_map: HashMap<String, String>,
    remote_urls: Vec<String>,
    cache_path: String,
    fs_utils: Box<dyn FSUtils>,
}

impl GitImpl {
    pub fn new(
        remote_urls: Vec<String>,
        package_map: HashMap<String, String>,
        cache_path: String,
        fs_utils: Box<dyn fs_utils::FSUtils>,
    ) -> Self {
        Self {
            package_map,
            remote_urls,
            cache_path,
            fs_utils,
        }
    }

    fn is_valid_semver(s: &str) -> bool {
        let parts: Vec<&str> = s.split('.').collect();

        if parts.len() < 3 {
            return false;
        }

        for part in &parts {
            if !part.chars().all(|c| c.is_ascii_digit()) {
                return false;
            }
        }

        true
    }

    fn is_valid_commit_hash(s: &str) -> bool {
        let len = s.len();

        if !(7..=40).contains(&len) {
            return false;
        }

        s.chars().all(|c| c.is_ascii_hexdigit())
    }

    fn is_not_semver_or_commit_hash(s: &str) -> bool {
        !(GitImpl::is_valid_semver(s) || GitImpl::is_valid_commit_hash(s))
    }

    /// Whether the clone at `repo_dest` was fetched within `interval`. Every pull rewrites
    /// `.git/FETCH_HEAD`; a clone that was never pulled only has `.git/HEAD` from the clone.
    fn fetched_within(repo_dest: &Path, interval: Duration) -> bool {
        let git_dir = repo_dest.join(".git");

        ["FETCH_HEAD", "HEAD"]
            .iter()
            .find_map(|name| fs::metadata(git_dir.join(name)).ok())
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age < interval)
    }

    /// Pulls a cached project clone unless it's pinned to a tag or commit, or was fetched recently.
    fn update_cached_clone(repo_dest: &str, remote_tag: Option<&str>) {
        if remote_tag.is_some_and(|tag| !GitImpl::is_not_semver_or_commit_hash(tag)) {
            info!("skipping git pull on {repo_dest}; ref: {remote_tag:?}; because either remote ref isn't defined or reference is a commit hash or a semver tag");
            return;
        }

        if GitImpl::fetched_within(Path::new(repo_dest), PULL_INTERVAL) {
            info!("skipping git pull on {repo_dest}; fetched within {PULL_INTERVAL:?}");
            return;
        }

        match Command::new("git")
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(["-C", repo_dest, "pull"])
            .output()
        {
            Ok(_) => info!("{repo_dest}: successfully updated using git clone"),
            Err(err) => error!("error using git clone inside: {repo_dest}; got: {err:?}"),
        }
    }

    /// Clone URLs to try for a component, in order. The workspace's own remotes come first so
    /// components use the same protocol and credentials as project includes.
    fn component_clone_urls(&self, component_info: &ComponentInfo) -> Vec<String> {
        let host = &component_info.host;
        let project = &component_info.project;

        let mut urls: Vec<String> = self
            .remote_urls
            .iter()
            .filter(|remote| ParserUtils::bare_host(remote) == host)
            .map(|remote| format!("{remote}{project}"))
            .collect();

        for url in [
            format!("https://{host}/{project}"),
            format!("git@{host}:{project}"),
        ] {
            if !urls.contains(&url) {
                urls.push(url);
            }
        }

        urls
    }

    fn clone_component_repo(&self, repo_dest: &str, component_info: &ComponentInfo) {
        let repo_dest_path = std::path::Path::new(&repo_dest);

        info!("repo_path: {}", repo_dest_path.display());

        if repo_dest_path.exists() {
            let mut repo_contents = match repo_dest_path.read_dir() {
                Ok(contents) => contents,
                Err(err) => {
                    error!("error reading repo contents; got err: {err}");
                    return;
                }
            };

            if repo_contents.next().is_some() {
                info!("repo contents exist");

                return;
            }
        }

        for url in self.component_clone_urls(component_info) {
            let output = Command::new("git")
                .env("GIT_TERMINAL_PROMPT", "0")
                .args([
                    "clone",
                    "--depth",
                    "1",
                    "--branch",
                    &component_info.version,
                    &url,
                    repo_dest,
                ])
                .output();

            match output {
                Ok(out) if out.status.success() => {
                    info!("successfully cloned {url} to: {repo_dest}");
                    return;
                }
                Ok(out) => error!(
                    "error cloning {url}; got: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
                Err(err) => error!("error running git clone for {url}; got: {err}"),
            }

            // An empty directory left behind would make the next attempt look like a cached clone.
            if repo_dest_path.exists() {
                if let Err(err) = fs::remove_dir_all(repo_dest_path) {
                    error!("error removing failed clone at {repo_dest}; got: {err}");
                    return;
                }
            }
        }
    }

    fn get_clone_repo_destination(
        cache_path: &str,
        remote_pkg: &str,
        remote_tag: Option<&str>,
    ) -> anyhow::Result<String> {
        let mut path = Path::new(cache_path).join(remote_pkg);

        // if we have a tag, add it to the path
        if let Some(tag) = remote_tag {
            path = path.join(tag);
        } else {
            path = path.join(DEFAULT_BRANCH_SUBFOLDER);
        }

        match path.to_str() {
            Some(path) => Ok(path.to_string()),
            None => Err(anyhow::anyhow!("invalid path")),
        }
    }
}

impl Git for GitImpl {
    fn clone_repo(&self, repo_dest: &str, remote_tag: Option<&str>, remote_pkg: &str) {
        let repo_dest_path = std::path::Path::new(repo_dest);

        info!("repo_path: {}", repo_dest_path.display());

        if repo_dest_path.exists() {
            let mut repo_contents = match repo_dest_path.read_dir() {
                Ok(contents) => contents,
                Err(err) => {
                    error!("error reading repo contents; got err: {err}");
                    return;
                }
            };

            if repo_contents.next().is_some() {
                info!("repo contents exist");
                GitImpl::update_cached_clone(repo_dest, remote_tag);

                return;
            }
        }

        let remotes = match self.package_map.get(remote_pkg) {
            Some(host) => vec![host.clone()],
            None => self.remote_urls.clone(),
        };

        info!("got git host: {remotes:?}");

        for origin in remotes {
            match Command::new("git")
                .args(
                    ["clone", "--depth", "1"]
                        .into_iter()
                        .chain(remote_tag.into_iter().flat_map(|tag| {
                            // Reference/commit can't be checked out this way
                            if GitImpl::is_valid_commit_hash(tag) {
                                vec![]
                            } else {
                                vec!["--branch", tag]
                            }
                        }))
                        .chain([&format!("{origin}{remote_pkg}"), repo_dest]),
                )
                .output()
            {
                Ok(out) if !out.status.success() => {
                    error!(
                        "error cloning {origin}{remote_pkg}; got: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                }
                Ok(ok) => {
                    info!("successfully cloned to : {repo_dest}; got: {ok:?}");
                    if let Some(tag) = remote_tag {
                        if GitImpl::is_valid_commit_hash(tag) {
                            match Command::new("git")
                                .args(["fetch", "--depth", "1", &origin, tag])
                                .output()
                            {
                                Ok(_) => {
                                    info!("successfully fetched referenced commit: {repo_dest} @ {tag};");

                                    info!("trying to checkout the commit");
                                    match Command::new("git")
                                        .args(["-C", repo_dest, "checkout", tag])
                                        .output()
                                    {
                                        Ok(_) => {
                                            info!(
                                                "successfully checkout repo: {repo_dest} @ {tag}"
                                            );
                                        }
                                        Err(err) => {
                                            error!("error checking out repo: {repo_dest} @ {tag}; got err: {err}");
                                            fs::remove_dir_all(repo_dest)
                                                .expect("should be able to remove");
                                        }
                                    }
                                }
                                Err(err) => {
                                    error!("error fetching referenced commit repo: {repo_dest} @ {tag}; got err: {err}");
                                    fs::remove_dir_all(repo_dest)
                                        .expect("should be able to remove");
                                }
                            }
                        }
                    }
                    break;
                }
                Err(err) => {
                    error!("error cloning to: {repo_dest}, got: {err:?}");

                    let dest = path::Path::new(repo_dest);
                    if dest.exists() {
                        fs::remove_dir_all(dest).expect("should be able to remove");
                    }
                }
            }
        }
    }

    fn fetch_remote_repository(
        &self,
        remote_pkg: &str,
        remote_tag: Option<&str>,
        remote_files: ProjectFile,
    ) -> anyhow::Result<Vec<GitlabFile>> {
        let files = match remote_files {
            ProjectFile::Multi(files) => files,
            ProjectFile::Single(single) => vec![single],
        };

        if remote_pkg.is_empty() || files.is_empty() {
            return Ok(vec![]);
        }

        self.fs_utils.create_dir_all(&self.cache_path)?;

        let repo_dest =
            GitImpl::get_clone_repo_destination(&self.cache_path, remote_pkg, remote_tag)?;

        self.clone_repo(repo_dest.as_str(), remote_tag, remote_pkg);

        let files = files
            .iter()
            .filter_map(|file| {
                // TODO: dirty hack, fix it when time
                let file = prepend_if_needed(file, '/');
                let file_path = format!("{repo_dest}{file}");
                debug!("filepath: {file_path}");

                let content = match std::fs::read_to_string(&file_path) {
                    Ok(content) => content,
                    Err(err) => {
                        error!("error reading content from: {file_path}; got err {err}");
                        return None;
                    }
                };

                let uri = match Url::parse(format!("file://{file_path}").as_str()) {
                    Ok(uri) => uri,
                    Err(err) => {
                        error!("error generating uri; got err {err}");
                        return None;
                    }
                };

                Some(GitlabFile {
                    path: uri.to_string(),
                    content,
                })
            })
            .collect();

        Ok(files)
    }

    fn fetch_remote(&self, url: Url) -> anyhow::Result<GitlabFile> {
        let remote_cache_path = format!("{}remotes", &self.cache_path);
        self.fs_utils.create_dir_all(&remote_cache_path)?;

        let file_hash = parser_utils::ParserUtils::remote_path_to_hash(url.as_str());
        let file_name_pattern = format!("_{file_hash}.yaml");

        let dir_entry = fs::read_dir(&remote_cache_path)?
            .filter_map(Result::ok)
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains(&file_name_pattern)
            });

        let (existing_name_full, file_path) = dir_entry
            .map(|entry| {
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    entry.path(),
                )
            })
            .unzip();

        // Extracting etag from the filename
        let existing_etag = existing_name_full
            .as_ref()
            .and_then(|name| name.split('_').next())
            .map(String::from);

        let client = Client::builder().timeout(Duration::from_secs(4)).build()?;

        let mut req = client.get(url);
        if let Some(etag) = &existing_etag {
            req = req.header(IF_NONE_MATCH, format!("\"{etag}\""));
        }

        let response = req.send()?;

        if response.status() == StatusCode::NOT_MODIFIED {
            let fpath = file_path.expect("File path must exist for NOT_MODIFIED response");
            let content = fs::read_to_string(&fpath)?;

            info!("CACHED");

            Ok(GitlabFile {
                path: format!("file://{}", fpath.to_str().unwrap()),
                content,
            })
        } else {
            info!("NOT CACHED");

            let headers = response.headers().clone();
            let etag = headers.get("etag").unwrap().to_str()?;
            let text = response.text()?;

            let path = format!(
                "{}/{}_{}.yaml",
                remote_cache_path,
                parser_utils::ParserUtils::strip_quotes(etag),
                file_hash
            );

            let mut file = File::create(&path)?;
            file.write_all(text.as_bytes())?;

            Ok(GitlabFile {
                path,
                content: text,
            })
        }
    }

    fn fetch_remote_component(
        &self,
        component_info: ComponentInfo,
    ) -> anyhow::Result<GitlabElement> {
        // TODO: handle slashes correctly..

        let repo_dest = ParserUtils::get_component_dest_dir(&self.cache_path, &component_info);
        self.fs_utils.create_dir_all(&repo_dest)?;

        self.clone_component_repo(repo_dest.as_str(), &component_info);
        ParserUtils::get_component(&repo_dest, &component_info.component)
    }

    fn get_project_remote_uris(&self) -> Vec<String> {
        self.remote_urls.clone()
    }
}

fn prepend_if_needed(input: &str, character: char) -> String {
    if input.starts_with(character) {
        input.to_string()
    } else {
        format!("{character}{input}")
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use fs_utils::MockFSUtils;

    use super::*;

    fn touch(path: &Path, age: Duration) {
        let file = File::create(path).unwrap();
        file.set_modified(SystemTime::now() - age).unwrap();
    }

    #[test]
    fn test_fetched_within() {
        let repo = tempfile::tempdir().unwrap();
        let git_dir = repo.path().join(".git");

        // not a clone at all
        assert!(!GitImpl::fetched_within(repo.path(), PULL_INTERVAL));

        fs::create_dir(&git_dir).unwrap();
        touch(&git_dir.join("HEAD"), Duration::from_secs(60));
        assert!(GitImpl::fetched_within(repo.path(), PULL_INTERVAL));

        touch(&git_dir.join("HEAD"), Duration::from_secs(3 * 60 * 60));
        assert!(!GitImpl::fetched_within(repo.path(), PULL_INTERVAL));

        // a recent pull wins over an old clone
        touch(&git_dir.join("FETCH_HEAD"), Duration::from_secs(60));
        assert!(GitImpl::fetched_within(repo.path(), PULL_INTERVAL));
    }

    #[test]
    fn test_component_clone_urls() {
        let git = GitImpl::new(
            vec![
                "https://git.example.com/".to_string(),
                "git@git.example.com:".to_string(),
            ],
            HashMap::new(),
            String::new(),
            Box::new(MockFSUtils::new()),
        );
        let component = |host: &str| ComponentInfo {
            host: host.to_string(),
            project: "group/comp".to_string(),
            component: "job".to_string(),
            version: "1.0.0".to_string(),
        };

        assert_eq!(
            git.component_clone_urls(&component("git.example.com")),
            vec![
                "https://git.example.com/group/comp",
                "git@git.example.com:group/comp",
            ]
        );
        assert_eq!(
            git.component_clone_urls(&component("gitlab.com")),
            vec!["https://gitlab.com/group/comp", "git@gitlab.com:group/comp"]
        );
    }

    #[test]
    fn test_get_clone_repo_destination_no_tag() {
        let cache_path = "/home/test/.cache/gitlab-ci-ls/";
        let remote_pkg = "repo/project";
        let tag = None;

        let repo_dest = GitImpl::get_clone_repo_destination(cache_path, remote_pkg, tag);
        assert_eq!(
            repo_dest.unwrap(),
            "/home/test/.cache/gitlab-ci-ls/repo/project/default"
        );
    }

    #[test]
    fn test_get_clone_repo_destination_with_tag() {
        let cache_path = "/home/test/.cache/gitlab-ci-ls/";
        let remote_pkg = "repo/project";
        let tag = Some("1.0.0");

        let repo_dest = GitImpl::get_clone_repo_destination(cache_path, remote_pkg, tag);
        assert_eq!(
            repo_dest.unwrap(),
            "/home/test/.cache/gitlab-ci-ls/repo/project/1.0.0"
        );
    }
}
