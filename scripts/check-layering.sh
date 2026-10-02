#!/usr/bin/env bash
# Changing either policy variable requires a lead decision.
OS_FREE=(crosspane-types crosspane-protocol crosspane-security crosspane-input crosspane-media crosspane-platform crosspane-engine crosspane-installer-core)
PLATFORM_BINDINGS='^(windows|windows-sys|windows-core|objc2.*|block2|dispatch2?|core-foundation(-sys)?|core-graphics.*|cocoa.*|wayland-.*|smithay-client-toolkit|ashpd|pipewire(-sys)?|reis|x11rb.*|x11-dl|xkbcommon.*|libudev.*|evdev.*)$'

set -euo pipefail

repo_root=$(cd -- "${BASH_SOURCE[0]%/*}/.." && pwd -P)
cd -- "$repo_root"
os_free_json=$(printf '%s\n' "${OS_FREE[@]}" | jq -R . | jq -s .)
metadata=$(cargo metadata --offline --format-version 1 --no-deps)
failed=0

report() {
    printf '%s\n' "$1"
    failed=1
}

# Use declarations for direct dependencies, including target-specific ones.
# Match workspace paths as well as names so registry names cannot be confused
# with workspace members. Cargo identifies normal dependencies with null.
violations=$(jq -r --argjson os_free "$os_free_json" '
    . as $metadata
    | [.packages[]
       | select(.id as $id | $metadata.workspace_members | index($id))] as $members
    | (
        $members[] as $package
        | select($package.name as $name | $os_free | index($name))
        | $package.dependencies[]
        | select(.kind == null or .kind == "build")
        | . as $dependency
        | $members[]
        | select(.name == $dependency.name
                 and (.manifest_path | rtrimstr("/Cargo.toml")) == $dependency.path)
        | select(.name as $name | $os_free | index($name) | not)
        | "\($package.name): rule a: \($dependency.kind // "normal") dependency on non-OS-free workspace member \(.name)"
      ), (
        $members[]
        | select(.name == "crosspane-testkit")
        | .dependencies[]
        | select(.kind == null or .kind == "build")
        | select(.name | startswith("crosspane-platform-"))
        | "crosspane-testkit: rule d: \(.kind // "normal") dependency on platform adapter \(.name)"
      )
' <<< "$metadata")
if [[ -n "$violations" ]]; then
    report "$violations"
fi

os_cfg_pattern='target_os|target_family|target_vendor|cfg[[:space:]]*\([[:space:]]*(unix|windows)[[:space:]]*\)'

# Bash recursion avoids relying on find/grep or Bash 4 features on macOS.
check_sources() {
    local crate=$1 path=$2 entry content
    if [[ -d "$path" ]]; then
        for entry in "$path"/* "$path"/.[!.]* "$path"/..?*; do
            [[ -e "$entry" ]] || continue
            check_sources "$crate" "$entry"
        done
    elif [[ -f "$path" ]]; then
        content=$(< "$path")
        if [[ $content =~ $os_cfg_pattern ]]; then
            report "$crate: rule c: OS configuration in ${path#"$repo_root"/}"
        fi
    fi
}

os_packages=$(jq -r --argjson os_free "$os_free_json" '
    .packages[]
    | select(.name as $name | $os_free | index($name))
    | [.name, .manifest_path] | @tsv
' <<< "$metadata")
while IFS=$'\t' read -r crate manifest; do
    while IFS= read -r line || [[ -n "$line" ]]; do
        if [[ $line =~ ^[[:space:]]*\[target\. ]]; then
            report "$crate: rule b: target table in Cargo.toml"
            break
        fi
    done < "$manifest"
    crate_dir=${manifest%/*}
    for source in src tests benches examples build.rs; do
        check_sources "$crate" "$crate_dir/$source"
    done
done <<< "$os_packages"

for target in x86_64-unknown-linux-gnu aarch64-apple-darwin; do
    metadata=$(cargo metadata --offline --format-version 1 --filter-platform "$target")
    violations=$(jq -r --argjson os_free "$os_free_json" \
        --arg bindings "$PLATFORM_BINDINGS" --arg target "$target" '
        # Walk only normal/build edges at every hop; dev edges stay exempt.
        # The visited set also prevents repeated traversal of shared subgraphs.
        def closure($edges):
            {seen: [], pending: [.]}
            | until(.pending | length == 0;
                .pending[0] as $id
                | .pending = .pending[1:]
                | if (.seen | index($id)) != null then .
                  else .seen += [$id] | .pending += ($edges[$id] // [])
                  end)
            | .seen[];
        . as $metadata
        | (.packages | map({key: .id, value: .}) | from_entries) as $packages
        | (.resolve.nodes | map({key: .id, value: [
            .deps[]
            | select(any(.dep_kinds[]; .kind == null or .kind == "build"))
            | .pkg
          ]}) | from_entries) as $edges
        | $metadata.packages[] as $package
        | select($package.name as $name | $os_free | index($name))
        | ($package.id | closure($edges)) as $dependency
        | $packages[$dependency].name as $name
        | select($name | test($bindings))
        | "\($package.name): rule e: transitive normal/build platform binding \($name) (\($target))"
    ' <<< "$metadata")
    if [[ -n "$violations" ]]; then
        report "$violations"
    fi
done

if (( failed )); then
    exit 1
fi
printf 'layering OK: %s OS-free crates checked (linux, macos)\n' "${#OS_FREE[@]}"
