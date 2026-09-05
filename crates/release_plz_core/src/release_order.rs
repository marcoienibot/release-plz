use cargo_metadata::{Dependency, DependencyKind, Package};
use tracing::debug;

/// Return packages in an order they can be released.
/// In the result, the packages are placed after all their dependencies.
/// Return an error if a circular dependency is detected.
pub fn release_order<'a>(packages: &'a [&Package]) -> anyhow::Result<Vec<&'a Package>> {
    // Normal/build dependencies remain mandatory. Versioned dev dependencies
    // are preferences: an existing registry release may satisfy a dev cycle.
    order_with_dev_edges(packages, &[])?;
    let edges: Vec<_> = packages
        .iter()
        .flat_map(|package| {
            package
                .dependencies
                .iter()
                .filter(|dependency| {
                    should_dep_be_released_before(dependency, package)
                        || (dependency.kind == DependencyKind::Development
                            && !dependency.req.comparators.is_empty())
                })
                .map(|dependency| (package.name.to_string(), dependency.name.clone()))
        })
        .collect();
    // Discard every optional edge in a cycle, rather than greedily selecting one:
    // selecting half a dev cycle could reverse a previously working release order.
    let dev_edges: Vec<_> = packages
        .iter()
        .flat_map(|package| {
            package
                .dependencies
                .iter()
                .filter(|dependency| {
                    dependency.kind == DependencyKind::Development
                        && !dependency.req.comparators.is_empty()
                        && !should_dep_be_released_before(dependency, package)
                        && !reachable(
                            &dependency.name,
                            package.name.as_str(),
                            &edges,
                            &mut Vec::new(),
                        )
                })
                .map(|dependency| (package.name.to_string(), dependency.name.clone()))
        })
        .collect();
    let order = order_with_dev_edges(packages, &dev_edges)?;
    debug!(
        "Release order: {:?}",
        order.iter().map(|p| &p.name).collect::<Vec<_>>()
    );
    Ok(order)
}

fn reachable(
    from: &str,
    target: &str,
    edges: &[(String, String)],
    visited: &mut Vec<String>,
) -> bool {
    if from == target {
        return true;
    }
    if visited.iter().any(|name| name == from) {
        return false;
    }
    visited.push(from.to_string());
    edges
        .iter()
        .filter(|(source, _)| source == from)
        .any(|(_, destination)| reachable(destination, target, edges, visited))
}

fn order_with_dev_edges<'a>(
    packages: &[&'a Package],
    dev_edges: &[(String, String)],
) -> anyhow::Result<Vec<&'a Package>> {
    let mut order = Vec::new();
    let mut passed = Vec::new();
    for package in packages {
        release_order_inner(packages, package, &mut order, &mut passed, dev_edges)?;
    }
    Ok(order)
}

/// The `passed` argument is used to track packages that you already visited to
/// detect circular dependencies.
fn release_order_inner<'a>(
    packages: &[&'a Package],
    pkg: &'a Package,
    order: &mut Vec<&'a Package>,
    passed: &mut Vec<&'a Package>,
    dev_edges: &[(String, String)],
) -> anyhow::Result<()> {
    if is_package_in(pkg, order) {
        return Ok(());
    }
    passed.push(pkg);

    for d in &pkg.dependencies {
        // Check if the dependency is part of the packages we are releasing.
        if let Some(dep) = packages.iter().find(|p| {
            d.name == *p.name
              // Exclude the current package.
              && p.name != pkg.name
              && (should_dep_be_released_before(d, pkg)
                || dev_edges.iter().any(|(from, to)| from == pkg.name.as_str() && to == &d.name))
        }) {
            anyhow::ensure!(
                !is_package_in(dep, passed),
                "Circular dependency detected: {} -> {}",
                dep.name,
                pkg.name,
            );
            release_order_inner(packages, dep, order, passed, dev_edges)?;
        }
    }

    order.push(pkg);
    passed.pop();
    Ok(())
}

/// Return true if the package is part of a packages array.
/// This function exists because `package.contains(pkg)` is expensive,
/// because it compares the whole package struct.
fn is_package_in(pkg: &Package, packages: &[&Package]) -> bool {
    packages.iter().any(|p| p.name == pkg.name)
}

/// Check if the dependency is enabled in features.
fn is_dep_in_features(pkg: &Package, dep: &str) -> bool {
    pkg.features
        // Discard features name.
        .values()
        // Any feature contains the dependency in the format `dep/feature`.
        .any(|enabled_features| {
            enabled_features
                .iter()
                .filter_map(|feature| feature.split_once('/').map(|split| split.0))
                .any(|enabled_dependency| enabled_dependency == dep)
        })
}

/// Check if the dependency should be released before the current package.
fn should_dep_be_released_before(dep: &Dependency, pkg: &Package) -> bool {
    // Ignore development dependencies. They don't need to be published before the current package...
    matches!(dep.kind, DependencyKind::Normal | DependencyKind::Build)
      // ...unless they are in features. In fact, `cargo-publish` compiles crates that are in features
      // and dev-dependencies, even if they are not present in normal dependencies.
      || is_dep_in_features(pkg, &dep.name)
}

#[cfg(test)]
mod tests {
    use fake_package::{FakeDependency, FakePackage};

    use super::*;
    use crate::publishable_packages_from_manifest;

    // Test the package release order in the release-plz workspace itself.
    #[test]
    fn workspace_release_order_is_correct() {
        let public_packages = publishable_packages_from_manifest("../../Cargo.toml").unwrap();
        let pkgs = &public_packages.iter().collect::<Vec<_>>();
        assert_eq!(
            order(pkgs),
            [
                "cargo_utils",
                "git_cmd",
                "test_logs",
                "next_version",
                "release_plz_core",
                "release-plz"
            ]
        );
    }

    /// Package
    fn pkg(name: &str, deps: &[FakeDependency]) -> Package {
        FakePackage::new(name)
            .with_dependencies(deps.to_vec())
            .into()
    }

    /// Dependency
    fn dep(name: &str) -> FakeDependency {
        FakeDependency::new(name)
    }

    /// Development dependency
    fn dev_dep(name: &str) -> FakeDependency {
        FakeDependency::new(name).dev()
    }

    fn order<'a>(pkgs: &'a [&'a Package]) -> Vec<&'a str> {
        release_order(pkgs)
            .unwrap()
            .iter()
            .map(|p| p.name.as_str())
            .collect()
    }

    #[test]
    fn versioned_dev_dependencies_are_preferred_before_dependents() {
        let pkgs = [&pkg("a", &[dev_dep("b")]), &pkg("b", &[])];
        assert_eq!(order(&pkgs), ["b", "a"]);
    }

    #[test]
    fn unversioned_dev_dependencies_do_not_change_order() {
        let mut a = pkg("a", &[dev_dep("b")]);
        a.dependencies[0].req = "*".parse().unwrap();
        let b = pkg("b", &[]);
        assert_eq!(order(&[&a, &b]), ["a", "b"]);
    }

    #[test]
    fn a_pure_dev_cycle_preserves_the_existing_order() {
        let a = pkg("a", &[dev_dep("b")]);
        let b = pkg("b", &[dev_dep("a")]);
        assert_eq!(order(&[&a, &b]), ["a", "b"]);
        assert_eq!(order(&[&b, &a]), ["b", "a"]);
    }

    #[test]
    fn a_dev_cycle_does_not_discard_other_dev_ordering() {
        let a = pkg("a", &[dev_dep("b"), dev_dep("c")]);
        let b = pkg("b", &[dep("a")]);
        let c = pkg("c", &[]);
        // b must follow a; c can still be published before a.
        assert_eq!(order(&[&a, &b, &c]), ["c", "a", "b"]);
    }

    // Diagrams created with https://asciiflow.com/

    /// A─►B
    #[test]
    fn single_package_is_returned() {
        let pkgs = [&pkg("a", &[dep("b")])];
        assert_eq!(order(&pkgs), ["a"]);
    }

    /// ┌──┐
    /// │  ▼
    /// A  B
    /// ▲  │
    /// └──┘
    #[test]
    fn two_packages_cycle_is_detected() {
        let pkgs = [&pkg("a", &[dep("b")]), &pkg("b", &[dep("a")])];
        expect_test::expect!["Circular dependency detected: a -> b"]
            .assert_eq(&release_order(&pkgs).unwrap_err().to_string());
    }

    /// ┌──┐
    /// │  ▼
    /// A  B (dev dependency)
    /// ▲  │
    /// └──┘
    #[test]
    fn two_packages_dev_cycle_is_ok() {
        let pkgs = [&pkg("a", &[dev_dep("b")]), &pkg("b", &[dep("a")])];
        assert_eq!(order(&pkgs), ["a", "b"]);

        // check if the order of the vector matters.
        let pkgs = [&pkg("b", &[dep("a")]), &pkg("a", &[dev_dep("b")])];
        assert_eq!(order(&pkgs), ["a", "b"]);
    }

    /// ┌─────┐
    /// ▼     │
    /// A────►B
    /// │     ▲
    /// └─►C──┘
    #[test]
    fn three_packages_cycle_is_detected() {
        let pkgs = [
            &pkg("a", &[dep("b")]),
            &pkg("a", &[dep("c")]),
            &pkg("b", &[dep("a")]),
            &pkg("c", &[dep("b")]),
        ];
        expect_test::expect!["Circular dependency detected: a -> b"]
            .assert_eq(&release_order(&pkgs).unwrap_err().to_string());
    }

    /// A────►C
    /// │     ▲
    /// └─►B──┘
    #[test]
    fn three_packages_are_ordered() {
        let pkgs = [
            &pkg("a", &[dep("b")]),
            &pkg("b", &[dep("c")]),
            &pkg("c", &[]),
        ];
        assert_eq!(order(&pkgs), ["c", "b", "a"]);
    }

    /// ┌──┐
    /// │  ▼
    /// A  B (dev dependency)
    /// ▲  │
    /// └──┘
    #[test]
    fn two_packages_dev_cycle_with_package_in_features_is_detected() {
        let mut a = pkg("a", &[dev_dep("b")]);
        a.features = [("my_feat".to_string(), vec!["b/feat".to_string()])].into();
        let pkgs = [&a, &pkg("b", &[dep("a")])];
        expect_test::expect!["Circular dependency detected: a -> b"]
            .assert_eq(&release_order(&pkgs).unwrap_err().to_string());
    }

    /// ┌──┐
    /// │  ▼
    /// A  B (dev dependency)
    /// ▲  │
    /// └──┘
    #[test]
    fn two_packages_dev_cycle_with_random_feature_is_ok() {
        let mut a = pkg("a", &[dev_dep("b")]);
        a.features = [(
            "my_feat".to_string(),
            vec!["b".to_string(), "rand/b".to_string()],
        )]
        .into();
        let pkgs = [&a, &pkg("b", &[dep("a")])];
        assert_eq!(order(&pkgs), ["a", "b"]);
    }
}
