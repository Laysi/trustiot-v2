//! A test container leaves no volume behind (Laysi/trustiot-v2#334).
//!
//! The images the tests start (Postgres, ClickHouse, Mosquitto) declare `VOLUME`s, so every
//! container gets anonymous volumes, and `rm --force` alone keeps them: a workstation once held
//! 2,011 of them and podman ran out of locks (`exceeded num_locks (2048)`). These tests read the
//! volumes a running container mounts, remove the container, and check that the volumes are
//! gone. They start a throwaway container with podman or docker; they are skipped without
//! either unless `CI` is set.

use std::process::{Command, Stdio};

use testkit::container::{Container, container_runtime, runtime_output, unavailable, unique_name};
use testkit::postgres::POSTGRES_IMAGE;

/// Starts a container from an image that declares a `VOLUME` (the Postgres image: its data
/// directory) and returns it with the names of the volumes it mounts.
fn start(runtime: &'static str) -> (Container, Vec<String>) {
    let container = Container {
        runtime,
        name: unique_name("volumes"),
    };
    // `sleep` instead of the server: only the mounts matter, and nothing has to start up.
    let _id = runtime_output(
        runtime,
        &[
            "run",
            "--detach",
            "--name",
            &container.name,
            "--entrypoint",
            "sleep",
            POSTGRES_IMAGE,
            "600",
        ],
    );
    let listed = runtime_output(
        runtime,
        &[
            "inspect",
            "--format",
            r#"{{range .Mounts}}{{if eq .Type "volume"}}{{.Name}} {{end}}{{end}}"#,
            &container.name,
        ],
    );
    let volumes: Vec<String> = listed.split_whitespace().map(str::to_owned).collect();
    assert!(
        !volumes.is_empty(),
        "the container mounts no volume, so the test would prove nothing: {listed:?}"
    );
    (container, volumes)
}

fn volume_exists(runtime: &str, volume: &str) -> bool {
    Command::new(runtime)
        .args(["volume", "inspect", volume])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The names in `volumes` that still exist; they are removed, so a failing test does not
/// leave them on the machine.
fn leftovers(runtime: &str, volumes: &[String]) -> Vec<String> {
    let left: Vec<String> = volumes
        .iter()
        .filter(|volume| volume_exists(runtime, volume))
        .cloned()
        .collect();
    for volume in &left {
        let _ = Command::new(runtime)
            .args(["volume", "rm", "--force", volume])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    left
}

#[test]
fn dropping_a_container_removes_its_volumes() {
    let Some(runtime) = container_runtime() else {
        unavailable("podman or docker");
        return;
    };
    let (container, volumes) = start(runtime);
    for volume in &volumes {
        assert!(
            volume_exists(runtime, volume),
            "{volume} exists while the container runs"
        );
    }

    drop(container);

    let left = leftovers(runtime, &volumes);
    assert!(
        left.is_empty(),
        "volumes left behind by the removed container: {left:?}"
    );
}

/// Proves the check above can fail: the removal the tests used before the fix keeps the
/// volumes, and this test finds them.
#[test]
fn removing_a_container_without_volumes_leaves_them_behind() {
    let Some(runtime) = container_runtime() else {
        unavailable("podman or docker");
        return;
    };
    let (container, volumes) = start(runtime);
    // The flag comes from a variable: `tools/repo-check` rejects the literal `"rm", "--force"`
    // without `"--volumes"`, and this is the one place that removes a container that way on
    // purpose.
    let force = "--force";
    let _ = runtime_output(runtime, &["rm", force, &container.name]);

    let left = leftovers(runtime, &volumes);
    assert_eq!(
        left, volumes,
        "a plain `rm --force` should keep the anonymous volumes"
    );
}
