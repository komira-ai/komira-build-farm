//! Where an image lives: the registry that serves its repository.

use kbf_types::ImageRef;

use crate::Error;

/// The registry address and repository path of an image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    /// The registry as the image names it (`docker.io`, `gcr.io`, `host:5000`).
    pub registry: String,
    /// The host and port requests go to: the registry, except Docker Hub's.
    pub endpoint: String,
    /// The repository path on the registry (`library/busybox`).
    pub repository: String,
}

/// Docker Hub's name in an image reference and the host that serves its API.
const DOCKER_HUB: &str = "docker.io";
const DOCKER_HUB_API: &str = "registry-1.docker.io";

impl Location {
    /// The location of `image`. A first path component with a dot or a port, or
    /// `localhost`, is the registry; without one the registry is Docker Hub, where a
    /// repository of one component is under `library/`.
    ///
    /// # Errors
    /// The repository has no path after the registry.
    pub fn of(image: &ImageRef) -> Result<Self, Error> {
        let name = image.repository();
        let (first, rest) = name.split_once('/').unwrap_or((name, ""));
        let names_registry =
            !rest.is_empty() && (first.contains(['.', ':']) || first == "localhost");
        let (registry, path) = if names_registry {
            (first, rest)
        } else {
            (DOCKER_HUB, name)
        };
        // `"".split('/')` is one empty component: a path with none is refused here too.
        if path.split('/').any(str::is_empty) {
            return Err(Error::Reference(name.to_owned()));
        }
        let hub = matches!(registry, DOCKER_HUB | "index.docker.io");
        let repository = if hub && !path.contains('/') {
            format!("library/{path}")
        } else {
            path.to_owned()
        };
        let endpoint = if hub { DOCKER_HUB_API } else { registry };
        Ok(Self {
            registry: registry.to_owned(),
            endpoint: endpoint.to_owned(),
            repository,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e";

    fn location(repository: &str) -> Result<Location, Error> {
        let image = ImageRef::parse(&format!("docker://{repository}@sha256:{HEX}")).expect("ref");
        Location::of(&image)
    }

    fn at(registry: &str, endpoint: &str, repository: &str) -> Location {
        Location {
            registry: registry.to_owned(),
            endpoint: endpoint.to_owned(),
            repository: repository.to_owned(),
        }
    }

    /// Catches Docker Hub names going to the wrong host or path: a one-component name
    /// is under `library/`, the API host is not the name, and a named registry is
    /// taken as written, with its port.
    #[test]
    fn registries_and_repositories_are_told_apart() {
        let hub = at("docker.io", "registry-1.docker.io", "library/busybox");
        assert_eq!(location("docker.io/library/busybox").unwrap(), hub);
        assert_eq!(location("docker.io/busybox").unwrap(), hub);
        assert_eq!(location("busybox").unwrap(), hub);
        assert_eq!(
            location("index.docker.io/busybox").unwrap().endpoint,
            "registry-1.docker.io"
        );
        assert_eq!(
            location("someone/tool").unwrap(),
            at("docker.io", "registry-1.docker.io", "someone/tool")
        );
        assert_eq!(
            location("gcr.io/project/tool").unwrap(),
            at("gcr.io", "gcr.io", "project/tool")
        );
        assert_eq!(
            location("localhost/x").unwrap(),
            at("localhost", "localhost", "x")
        );
        assert_eq!(
            location("127.0.0.1:5000/a/b").unwrap(),
            at("127.0.0.1:5000", "127.0.0.1:5000", "a/b")
        );
    }

    /// Catches a repository with an empty component reaching a URL.
    #[test]
    fn an_empty_path_component_is_refused() {
        for bad in ["gcr.io/", "docker.io//x", "a//b"] {
            assert!(
                matches!(location(bad), Err(Error::Reference(_))),
                "{bad}: {:?}",
                location(bad)
            );
        }
    }
}
