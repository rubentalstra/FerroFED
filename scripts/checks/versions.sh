#!/usr/bin/env bash
# SPDX-FileCopyrightText: Vernum Projecten B.V.
# SPDX-License-Identifier: BUSL-1.1
# Version-drift guard: the pin matrix, docs/VERSIONS.md, is the single source
# of truth (no specification governs this: our own design).
#
# Every file that repeats a pin must agree with the matrix. A check whose
# subject file is absent SKIPS LOUDLY with a printed reason, and gains teeth
# the moment the file appears.
#
#   1. specification pins  the Federation Tier with AQL, openEHR ITS-REST and
#                          openEHR AQL rows of the docs/architecture.md pin
#                          table against docs/VERSIONS.md, and each row against
#                          the crate constant it names (`FEDERATION_SPEC`,
#                          `ITS_REST`, `AQL`).
#   2. model crates        openehr-query and openehr-its across
#                          docs/architecture.md, docs/VERSIONS.md, and the root
#                          Cargo.toml [workspace.dependencies] requirement,
#                          and every family row against the crate constant
#                          it names (`OPENEHR_FAMILY`).
#   3. toolchain          rust-toolchain.toml channel, plus the root
#                          Cargo.toml edition, rust-version and resolver.
#   4. product version     CITATION.cff version against the docs/VERSIONS.md
#                          product-version row, and against the root Cargo.toml
#                          [workspace.package] version.
#   5. CI tool pins        the zizmor, actionlint, shellcheck, hadolint,
#                          kubeconform and lychee versions
#                          .github/workflows/ci.yml installs, the Kubernetes release and the schema
#                          commit kubeconform validates against, and the
#                          cargo-auditable, cargo-cyclonedx and syft versions
#                          the release workflows install.
#   6. docs toolchain      the mdBook, mdbook-toc and mdbook-mermaid defaults of
#                          .github/actions/docs-toolchain/action.yml.
#   7. testkit images      the PinnedImage constants of the testkit container
#                          harness against the docs/VERSIONS.md image rows.
#   8. vendored corpora    every docs/specs/*/PROVENANCE.md names the commit or
#                          tag its docs/VERSIONS.md corpus row pins, and the
#                          federation specification's provenance declares the
#                          version the specification row pins.
#   9. container images    the FROM of docker/Dockerfile against the base-image
#                          row, every digest-pinned compose.yaml image against
#                          a row naming the same reference, and the
#                          compose.yaml gateway tag default, the one image of
#                          the release asset deploy/compose/compose.yaml and
#                          the image tag of deploy/kubernetes/deployment.yaml
#                          against the product version.
#  10. licence             LICENSE is the Business Source License 1.1 and no
#                          first-party file claims MIT or Apache-2.0 as its
#                          own.
#  11. landing release     every "vX.Y.Z released" and "vX.Y.Z is the current
#                          release" on website/landing/index.html names the
#                          newest `## [x.y.z]` release of CHANGELOG.md.
#  12. book pins           every row of the pin table on the book page
#                          website/book/src/evaluate/versions.md restates
#                          the docs/VERSIONS.md rows its Item cell names.
#  13. metrics crates      the opentelemetry group moves as one, and each of
#                          its rows and the prometheus row matches the root
#                          Cargo.toml [workspace.dependencies] requirement.
#
# FerroFED's own database image gets a check of its own in the change that adds
# its first pin row.
#
# Usage:
#   scripts/checks/versions.sh
#   scripts/checks/versions.sh --self-test
#       Drives the specification-constant, landing-release and book-pin checks
#       against fixtures: an agreeing input passes and each drift fails.
#   Any other argument prints this usage and exits 2.
#
# Exit 0 = every present check agrees (skips are fine). Exit 1 = a real drift.
#
# No specification governs this file; it is FerroFED's own design.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"

matrix=docs/VERSIONS.md

fail=0
note() { printf '  %s\n' "$*"; }
bad() {
  printf '  DRIFT: %s\n' "$*" >&2
  fail=1
}

if [ ! -f "$matrix" ]; then
  echo "versions: $matrix is missing, and it is the source of truth" >&2
  exit 1
fi

# The first whitespace-separated token of the second cell of the markdown table
# row whose first cell is ITEM, with surrounding spaces and backticks removed.
pin_of() {
  awk -F'|' -v item="$1" '
    NF >= 3 {
      k = $2; v = $3
      gsub(/`/, "", k); gsub(/`/, "", v)
      gsub(/^[[:space:]]+|[[:space:]]+$/, "", k)
      gsub(/^[[:space:]]+|[[:space:]]+$/, "", v)
      if (k == item) { split(v, w, /[[:space:]]/); print w[1]; exit }
    }
  ' "$2"
}

# The whole second cell of the row whose first cell is ITEM, backticks removed.
pin_cell_of() {
  awk -F'|' -v item="$1" '
    NF >= 3 {
      k = $2; v = $3
      gsub(/`/, "", k); gsub(/`/, "", v)
      gsub(/^[[:space:]]+|[[:space:]]+$/, "", k)
      gsub(/^[[:space:]]+|[[:space:]]+$/, "", v)
      if (k == item) { print v; exit }
    }
  ' "$2"
}

# The value of KEY inside TOML table TABLE, unquoted.
toml_val() {
  awk -v table="$1" -v key="$2" '
    /^[[:space:]]*\[/ { h = $0; gsub(/[[:space:]]/, "", h); f = (h == table); next }
    f && $0 ~ "^[[:space:]]*" key "[[:space:]]*=" {
      if (match($0, /"[^"]*"/)) { print substr($0, RSTART + 1, RLENGTH - 2); exit }
      sub(/^[^=]*=[[:space:]]*/, "")
      gsub(/[[:space:]]/, "")
      print; exit
    }
  ' "$3"
}

# The version requirement of dependency NAME in the root Cargo.toml, in either
# the `name = "x.y.z"` or the `name = { version = "x.y.z" }` form.
manifest_req() {
  awk -v name="$1" '
    $0 ~ "^[[:space:]]*" name "[[:space:]]*=" {
      if (match($0, /version[[:space:]]*=[[:space:]]*"[^"]+"/)) {
        s = substr($0, RSTART, RLENGTH)
      } else if (match($0, /=[[:space:]]*"[^"]+"/)) {
        s = substr($0, RSTART, RLENGTH)
      } else { next }
      match(s, /"[^"]+"/)
      print substr(s, RSTART + 1, RLENGTH - 2); exit
    }
  ' "${2:-Cargo.toml}"
}

# The `default:` of composite-action input KEY, unquoted. An input key sits at
# two spaces of indentation and its own keys at four, which is what the exact
# prefix comparisons below rely on.
action_default() {
  awk -v key="  $1:" '
    $0 == key { inside = 1; next }
    inside && index($0, "    default:") == 1 {
      sub(/^[[:space:]]*default:[[:space:]]*/, "")
      gsub(/"/, "")
      gsub(/[[:space:]]/, "")
      print
      exit
    }
    inside && $0 ~ /^[^[:space:]]/ { exit }
  ' "$2"
}

# The whole third cell of the row whose first cell is ITEM: where the pin is
# repeated.
where_of() {
  awk -F'|' -v item="$1" '
    NF >= 4 {
      k = $2; v = $4
      gsub(/`/, "", k)
      gsub(/^[[:space:]]+|[[:space:]]+$/, "", k)
      gsub(/^[[:space:]]+|[[:space:]]+$/, "", v)
      if (k == item) { print v; exit }
    }
  ' "$2"
}

# spec_constant ITEM MATRIX BASE: the specification row ITEM names "the `NAME`
# constant of `CRATE`" in its third cell, and that constant, a `pub const NAME:
# &str` under BASE/crates/CRATE/src or BASE/app/CRATE/src, carries the version
# the row pins.
spec_constant() {
  local item=$1 file=$2 base=$3 want cell name crate dir found
  want="$(pin_of "$item" "$file")"
  cell="$(where_of "$item" "$file")"
  local named="s/.*the \`([A-Z][A-Z0-9_]*)\` constant of \`([a-z0-9-]+)\`.*/"
  name="$(sed -nE "${named}\\1/p" <<< "$cell")"
  crate="$(sed -nE "${named}\\2/p" <<< "$cell")"
  if [ -z "$want" ]; then
    bad "$file has no '$item' pin row"
    return
  fi
  if [ -z "$name" ] || [ -z "$crate" ]; then
    bad "$item: the $file row names no constant (the \`NAME\` constant of \`crate\`)"
    return
  fi
  dir=""
  [ -d "$base/crates/$crate/src" ] && dir="$base/crates/$crate/src"
  [ -d "$base/app/$crate/src" ] && dir="$base/app/$crate/src"
  if [ -z "$dir" ]; then
    bad "$item: $file names the $name constant of $crate, and no crate $crate exists"
    return
  fi
  found="$(grep -rhE "^pub const $name: &str = \"[^\"]*\";" "$dir" |
    sed -E 's/.*= "([^"]*)";.*/\1/' | sort -u || true)"
  if [ -z "$found" ]; then
    bad "$item: $crate has no pub const $name: &str"
  elif [ "$(printf '%s\n' "$found" | wc -l | tr -d '[:space:]')" != "1" ]; then
    bad "$item: $crate defines $name more than once ($(printf '%s' "$found" | tr '\n' ' '))"
  elif [ "$found" != "$want" ]; then
    bad "$item: $crate's $name is $found, $file pins $want"
  else
    note "OK: $item $want ($crate's $name agrees)"
  fi
}

# The newest released version of a Keep a Changelog file: its first
# `## [x.y.z]` heading, so `## [Unreleased]` never counts.
newest_release() {
  sed -nE 's/^## \[([0-9]+\.[0-9]+\.[0-9]+)\].*/\1/p' "$1" | head -n1
}

# landing_release PAGE CHANGELOG: every "vX.Y.Z released" and "vX.Y.Z is the
# current release" on PAGE names the newest release of CHANGELOG, and PAGE
# names it at least once. An "In vX.Y.Z" tag says where a feature shipped and
# is not a release claim.
landing_release() {
  local page=$1 log=$2 want found v count=0 stale=0
  want="$(newest_release "$log")"
  if [ -z "$want" ]; then
    bad "$log has no ## [x.y.z] release heading"
    return
  fi
  found="$(grep -oE 'v[0-9]+\.[0-9]+\.[0-9]+ (released|is the current release)' "$page" |
    sed -E 's/^v([0-9.]+) .*/\1/' || true)"
  if [ -z "$found" ]; then
    bad "$page names no current release; it should say v$want, the newest release in $log"
    return
  fi
  while IFS= read -r v; do
    count=$((count + 1))
    if [ "$v" != "$want" ]; then
      bad "$page says v$v is the current release, the newest release in $log is $want"
      stale=1
    fi
  done <<< "$found"
  [ "$stale" -eq 0 ] && note "OK: $page names v$want, the newest release, $count times"
  return 0
}

# claim_holds CELL LABEL VALUE: the pin cell CELL of a matrix row carries
# `LABEL VALUE` as two adjacent words. A VALUE of seven or more hex digits also
# matches as the prefix of the 40-hex commit that follows LABEL, so the book
# may abbreviate a commit.
claim_holds() {
  awk -v label="$2" -v value="$3" '
    {
      for (i = 1; i < NF; i++) {
        if ($i != label) continue
        w = $(i + 1); gsub(/[,.;:]+$/, "", w)
        if (w == value) { found = 1; exit }
        if (length(value) >= 7 && value ~ /^[0-9a-f]+$/ && w ~ /^[0-9a-f]+$/ && length(w) == 40 \
            && substr(w, 1, length(value)) == value) { found = 1; exit }
      }
    }
    END { exit !found }
  ' <<< "$1"
}

# book_pins PAGE MATRIX: every row of the `| Item | Pin | Why |` table on PAGE
# restates MATRIX. The Item cell names one or more matrix rows, comma-separated;
# the Pin cell is comma-separated claims. A claim with no digit and no backtick
# is prose and is skipped. A one-word claim is the pin of every named row, its
# first word. A two-word claim `LABEL VALUE` is the pin of the matrix row named
# LABEL with its first letter capitalised (`edition 2024`), or stands as those
# two words in the pin cell of every named row (`commit 7162d0c`). Any other
# claim is refused, so a version the guard cannot read never passes unread.
book_pins() {
  local page=$1 file=$2 rows row item_cell pin_cell items item i claims raw claim label value cap
  local -a names words
  rows="$(awk -F'|' '
    /^\|[[:space:]]*Item[[:space:]]*\|[[:space:]]*Pin[[:space:]]*\|/ { table = 1; next }
    table && /^\|[-|: ]+\|[[:space:]]*$/ { next }
    table && /^\|/ { print $2 "|" $3; next }
    table { exit }
  ' "$page")"
  if [ -z "$rows" ]; then
    bad "$page has no | Item | Pin | table to hold to $file"
    return 0
  fi
  local count=0 stale=0
  while IFS= read -r row; do
    count=$((count + 1))
    item_cell="${row%%|*}"
    pin_cell="${row#*|}"
    items="$(tr -d '`' <<< "$item_cell")"
    IFS=',' read -r -a names <<< "$items"
    for i in "${!names[@]}"; do
      item="$(sed -E 's/^[[:space:]]+|[[:space:]]+$//g' <<< "${names[$i]}")"
      names[i]="$item"
      if [ -z "$(pin_cell_of "$item" "$file")" ]; then
        bad "$page names '$item', which $file has no row for"
        stale=1
      fi
    done
    claims=0
    while IFS= read -r raw; do
      case "$raw" in
      *[0-9]* | *'`'*) ;;
      *) continue ;;
      esac
      claim="$(tr -d '`' <<< "$raw" | sed -E 's/^[[:space:]]+|[[:space:]]+$//g')"
      claims=$((claims + 1))
      read -r -a words <<< "$claim"
      case "${#words[@]}" in
      1)
        for item in "${names[@]}"; do
          value="$(pin_of "$item" "$file")"
          if [ -n "$value" ] && [ "$value" != "${words[0]}" ]; then
            bad "$page pins $item at ${words[0]}, $file pins $value"
            stale=1
          fi
        done
        ;;
      2)
        label="${words[0]}"
        value="${words[1]}"
        cap="$(printf '%s' "${label:0:1}" | tr '[:lower:]' '[:upper:]')${label:1}"
        if [ -n "$(pin_cell_of "$cap" "$file")" ]; then
          if [ "$(pin_of "$cap" "$file")" != "$value" ]; then
            bad "$page says $label $value, $file pins $cap at $(pin_of "$cap" "$file")"
            stale=1
          fi
        else
          for item in "${names[@]}"; do
            if ! claim_holds "$(pin_cell_of "$item" "$file")" "$label" "$value"; then
              bad "$page says $item is $label $value, which its $file row does not pin"
              stale=1
            fi
          done
        fi
        ;;
      *)
        bad "$page pins '${names[*]}' as '$claim', a claim the guard cannot read: write a pin or a LABEL VALUE pair"
        stale=1
        ;;
      esac
    done < <(tr ',' '\n' <<< "$pin_cell")
    if [ "$claims" -eq 0 ]; then
      bad "$page has a row for '${names[*]}' that pins nothing"
      stale=1
    fi
  done <<< "$rows"
  [ "$stale" -eq 0 ] && note "OK: all $count rows of $page agree with $file"
  return 0
}

# The self-test drives the two checks above against fixtures in a temporary
# directory: an agreeing input passes, and each kind of drift fails with its
# reason.
self_test() {
  local work out
  work="$(mktemp -d)"
  out="$work/out"
  # expect NAME WANT COMMAND...: COMMAND sets fail to WANT, and on a drift
  # prints a DRIFT line.
  expect() {
    local name=$1 want=$2
    shift 2
    fail=0
    "$@" > "$out" 2>&1
    if [ "$fail" -ne "$want" ]; then
      echo "versions: self-test failed: $name left fail=$fail, wanted $want." >&2
      cat "$out" >&2
      exit 1
    fi
    if [ "$want" -eq 1 ] && ! grep -q 'DRIFT:' "$out"; then
      echo "versions: self-test failed: $name failed without a DRIFT line." >&2
      exit 1
    fi
  }

  printf '%s\n' '## [Unreleased]' '' '## [0.0.4] - 2026-10-09' '' '## [0.0.3] - 2026-10-02' > "$work/CHANGELOG.md"
  printf '%s\n' '<p>v0.0.4 released. Signed.</p>' '<span>In v0.0.3</span>' '<p>v0.0.4 is the current release.</p>' > "$work/agree.html"
  printf '%s\n' '<p>v0.0.3 released. Signed.</p>' '<p>v0.0.4 is the current release.</p>' > "$work/stale-note.html"
  printf '%s\n' '<p>v0.0.4 released.</p>' '<p>v0.0.3 is the current release.</p>' > "$work/stale-panel.html"
  printf '%s\n' '<p>In v0.0.4</p>' > "$work/silent.html"
  printf '%s\n' '## [Unreleased]' > "$work/unreleased.md"
  expect "a landing page that names the newest release" 0 landing_release "$work/agree.html" "$work/CHANGELOG.md"
  expect "a stale release note" 1 landing_release "$work/stale-note.html" "$work/CHANGELOG.md"
  expect "a stale status panel" 1 landing_release "$work/stale-panel.html" "$work/CHANGELOG.md"
  expect "a landing page that names no release" 1 landing_release "$work/silent.html" "$work/CHANGELOG.md"
  expect "a changelog with no release" 1 landing_release "$work/agree.html" "$work/unreleased.md"

  mkdir -p "$work/tree/crates/spec-crate/src/inner" "$work/tree/app/app-crate/src"
  printf '%s\n' 'pub const SPEC: &str = "0.9.0";' > "$work/tree/crates/spec-crate/src/lib.rs"
  printf '%s\n' 'pub const WIRE: &str = "1.1.0";' 'pub const WIRE_PREFIX: &str = "/v1/";' > "$work/tree/app/app-crate/src/lib.rs"
  printf '%s\n' 'pub const TWICE: &str = "1.0.0";' > "$work/tree/crates/spec-crate/src/inner/mod.rs"
  printf '%s\n' 'pub const TWICE: &str = "2.0.0";' >> "$work/tree/crates/spec-crate/src/lib.rs"
  cat > "$work/matrix.md" <<'MATRIX'
| Item | Pin | Where it is repeated |
|---|---|---|
| Spec | 0.9.0 | `docs/architecture.md`, the `SPEC` constant of `spec-crate` |
| Wire | 1.1.0 | the `WIRE` constant of `app-crate` |
| Moved | 1.0.0 | the `SPEC` constant of `spec-crate` |
| Unnamed | 1.0.0 | `docs/architecture.md` |
| Missing | 1.0.0 | the `ABSENT` constant of `spec-crate` |
| Elsewhere | 1.0.0 | the `SPEC` constant of `no-such-crate` |
| Twice | 1.0.0 | the `TWICE` constant of `spec-crate` |
MATRIX
  expect "a spec row that agrees with its library constant" 0 spec_constant Spec "$work/matrix.md" "$work/tree"
  expect "a spec row that agrees with its app constant" 0 spec_constant Wire "$work/matrix.md" "$work/tree"
  expect "a spec row that disagrees with its constant" 1 spec_constant Moved "$work/matrix.md" "$work/tree"
  expect "a spec row that names no constant" 1 spec_constant Unnamed "$work/matrix.md" "$work/tree"
  expect "a spec row whose constant is absent" 1 spec_constant Missing "$work/matrix.md" "$work/tree"
  expect "a spec row whose crate is absent" 1 spec_constant Elsewhere "$work/matrix.md" "$work/tree"
  expect "a spec row whose constant is defined twice" 1 spec_constant Twice "$work/matrix.md" "$work/tree"
  expect "a matrix with no such row" 1 spec_constant Absent "$work/matrix.md" "$work/tree"

  cat > "$work/pins.md" <<'MATRIX'
| Item | Pin | Repeated in |
|---|---|---|
| Spec | 0.9.0 | `docs/architecture.md` |
| Spec source | `org/spec` commit `7162d0c760d23105d62a743bf0ad1073c45fdb85` | `scripts/vendor/spec.sh` |
| Package | `ihe.iti.pixm` version `3.1.0` from `packages.fhir.org` | `scripts/vendor/pixm.sh` |
| `crate-a` | 0.0.76 | the root `Cargo.toml` |
| `crate-b` | 0.0.76 | the root `Cargo.toml` |
| Rust toolchain | 1.98.1 | `rust-toolchain.toml` |
| Edition | 2024 | the root `Cargo.toml` |
MATRIX
  # Each fixture page holds the agreeing rows, then the one row its name says
  # is wrong; `agree` adds none.
  cat > "$work/book-rows" <<'ROWS'
| Spec | 0.9.0, release candidate | the governing text; 1.0 replaces it |
| Spec source | commit `7162d0c` | the vendored source |
| Package | `ihe.iti.pixm`, version 3.1.0 | the binding |
| `crate-a`, `crate-b` | 0.0.76 | one family |
| Rust toolchain | 1.98.1, edition 2024 | the toolchain |
ROWS
  local name extra
  while IFS='~' read -r name extra; do
    {
      printf '%s\n' '# Pinned versions' '' '| Item | Pin | Why |' '|---|---|---|'
      cat "$work/book-rows"
      [ -z "$extra" ] || printf '%s\n' "$extra"
      printf '%s\n' '' 'Prose after the table names 9.9.9 and is not a row.'
    } > "$work/$name.md"
  done <<'PAGES'
agree~
stale-pin~| Spec | 0.8.0 | the old text |
stale-member~| `crate-a`, `crate-b`, Spec | 0.0.76 | one family |
stale-label~| Rust toolchain | 1.98.1, edition 2021 | the toolchain |
stale-commit~| Spec source | commit `0123abc` | the vendored source |
short-commit~| Spec source | commit `7162d0` | the vendored source |
stale-version~| Package | `ihe.iti.pixm`, version 3.0.0 | the binding |
stale-package~| Package | `ihe.iti.pdqm`, version 3.1.0 | the binding |
unknown-item~| Rust | 1.98.1 | the toolchain |
unreadable~| Spec | 0.9.0 as of 2026-10-01 | the governing text |
pins-nothing~| Spec | release candidate | the governing text |
PAGES
  printf '%s\n' '# Pinned versions' '' 'No table here; 0.9.0 in prose.' > "$work/no-table.md"
  expect "a book table that restates the matrix" 0 book_pins "$work/agree.md" "$work/pins.md"
  expect "a book pin that moved" 1 book_pins "$work/stale-pin.md" "$work/pins.md"
  expect "a book row whose shared pin one member lacks" 1 book_pins "$work/stale-member.md" "$work/pins.md"
  expect "a labelled book pin that names another row" 1 book_pins "$work/stale-label.md" "$work/pins.md"
  expect "a book commit that is not the pinned one" 1 book_pins "$work/stale-commit.md" "$work/pins.md"
  expect "a book commit shorter than seven digits" 1 book_pins "$work/short-commit.md" "$work/pins.md"
  expect "a book package version that moved" 1 book_pins "$work/stale-version.md" "$work/pins.md"
  expect "a book package that is not the pinned one" 1 book_pins "$work/stale-package.md" "$work/pins.md"
  expect "a book row the matrix has no row for" 1 book_pins "$work/unknown-item.md" "$work/pins.md"
  expect "a book claim the guard cannot read" 1 book_pins "$work/unreadable.md" "$work/pins.md"
  expect "a book row that pins nothing" 1 book_pins "$work/pins-nothing.md" "$work/pins.md"
  expect "a book page with no pin table" 1 book_pins "$work/no-table.md" "$work/pins.md"

  rm -r "$work"
  echo "versions: self-test OK."
}

case "$#:${1:-}" in
0:) ;;
1:--self-test)
  self_test
  exit 0
  ;;
*)
  sed -n '/^# Usage:/,/^$/p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 2
  ;;
esac

echo "== specification pins (docs/architecture.md <-> $matrix)"
specs=("Federation Tier with AQL" "openEHR ITS-REST" "openEHR AQL")
if [ -f docs/architecture.md ]; then
  agreed=0
  for item in "${specs[@]}"; do
    arch="$(pin_of "$item" docs/architecture.md)"
    want="$(pin_of "$item" "$matrix")"
    if [ -z "$arch" ]; then
      bad "docs/architecture.md has no '$item' pin row"
    elif [ -z "$want" ]; then
      bad "$matrix has no '$item' pin row"
    elif [ "$arch" != "$want" ]; then
      bad "$item: docs/architecture.md says $arch, $matrix pins $want"
    else
      agreed=$((agreed + 1))
    fi
  done
  [ "$agreed" -eq "${#specs[@]}" ] && note "OK: all ${#specs[@]} specification pins agree"
else
  for item in "${specs[@]}"; do
    [ -n "$(pin_of "$item" "$matrix")" ] || bad "$matrix has no '$item' pin row"
  done
  note "no docs/architecture.md, skipped the comparison"
fi

echo "== specification constants ($matrix <-> the crate constant each row names)"
for item in "${specs[@]}"; do
  spec_constant "$item" "$matrix" .
done

echo "== model crate pins (docs/architecture.md <-> $matrix <-> Cargo.toml)"
# The openehr-* family is released in lockstep, so its rows are one group: a
# member that moves alone is drift even when its own file pair agrees.
family_pin=""
for crate in openehr-query openehr-its openehr-base openehr-rm openehr-sdt; do
  want="$(pin_of "$crate" "$matrix")"
  if [ -z "$want" ]; then
    bad "$matrix has no $crate row"
    continue
  fi
  if [ -z "$family_pin" ]; then
    family_pin="$want"
  elif [ "$want" != "$family_pin" ]; then
    bad "$crate: $matrix pins $want, the rest of the openehr-* family $family_pin; the family moves together"
  fi
  # The startup banner prints the family version from a crate constant,
  # which each family row names.
  spec_constant "$crate" "$matrix" .
  case "$crate" in
  openehr-query | openehr-its) ;;
  *)
    if [ -f Cargo.toml ]; then
      req="$(manifest_req "$crate")"
      if [ -z "$req" ]; then
        note "root Cargo.toml has no $crate requirement yet, skipped"
      elif [ "$req" != "$want" ]; then
        bad "$crate: root Cargo.toml requires $req, $matrix pins $want"
      else
        note "OK: $crate $want (root Cargo.toml agrees)"
      fi
    fi
    continue
    ;;
  esac
  if [ -f docs/architecture.md ]; then
    arch="$(pin_of "$crate" docs/architecture.md)"
    if [ -z "$arch" ]; then
      bad "docs/architecture.md has no $crate row"
      continue
    elif [ "$arch" != "$want" ]; then
      bad "$crate: docs/architecture.md says $arch, $matrix pins $want"
      continue
    fi
  fi
  if [ -f Cargo.toml ]; then
    req="$(manifest_req "$crate")"
    if [ -z "$req" ]; then
      note "root Cargo.toml has no $crate requirement yet, skipped"
    elif [ "$req" != "$want" ]; then
      bad "$crate: root Cargo.toml requires $req, $matrix pins $want"
    else
      note "OK: $crate $want (root Cargo.toml agrees)"
    fi
  else
    note "no root Cargo.toml yet, skipped the $crate requirement ($matrix pins $want)"
  fi
done
# The fuzz crate sits outside the workspace with its own lockfile, so it names
# the family by version and drifts unseen unless it is held to the same pin.
if [ -f fuzz/Cargo.toml ] && [ -n "$family_pin" ]; then
  for crate in openehr-query openehr-its openehr-base openehr-rm openehr-sdt; do
    req="$(manifest_req "$crate" fuzz/Cargo.toml)"
    if [ -z "$req" ]; then
      continue
    elif [ "$req" != "$family_pin" ]; then
      bad "$crate: fuzz/Cargo.toml requires $req, the openehr-* family is pinned at $family_pin"
    else
      note "OK: $crate $req (fuzz/Cargo.toml agrees)"
    fi
  done
fi

echo "== toolchain (rust-toolchain.toml and Cargo.toml <-> $matrix)"
if [ -f rust-toolchain.toml ]; then
  chan="$(toml_val "[toolchain]" channel rust-toolchain.toml)"
  want="$(pin_of "Rust toolchain" "$matrix")"
  if [ -z "$chan" ]; then
    bad "rust-toolchain.toml has no [toolchain] channel"
  elif [ -z "$want" ]; then
    bad "$matrix has no 'Rust toolchain' row"
  elif [ "$chan" != "$want" ]; then
    bad "toolchain: rust-toolchain.toml channel is $chan, $matrix pins $want"
  else
    note "OK: the toolchain is $chan"
  fi
else
  note "no rust-toolchain.toml yet, skipped"
fi

if [ -f Cargo.toml ]; then
  check_row() {
    local label="$1" found="$2" row="$3" want
    want="$(pin_of "$row" "$matrix")"
    if [ -z "$found" ]; then
      note "root Cargo.toml has no $label yet, skipped"
    elif [ -z "$want" ]; then
      bad "$matrix has no '$row' row"
    elif [ "$found" != "$want" ]; then
      bad "$label: root Cargo.toml says $found, $matrix pins $want"
    else
      note "OK: $label is $found"
    fi
  }
  check_row edition "$(toml_val "[workspace.package]" edition Cargo.toml)" "Edition"
  check_row rust-version "$(toml_val "[workspace.package]" rust-version Cargo.toml)" "MSRV"
  check_row resolver "$(toml_val "[workspace]" resolver Cargo.toml)" "Cargo resolver"
else
  note "no root Cargo.toml yet, skipped the edition, MSRV and resolver rows"
fi

echo "== product version (CITATION.cff <-> $matrix <-> Cargo.toml)"
want_product="$(pin_of "Product version" "$matrix")"
[ -n "$want_product" ] || bad "$matrix has no 'Product version' row"
if [ -f CITATION.cff ]; then
  cff="$(sed -nE 's/^version:[[:space:]]*//p' CITATION.cff | head -n1 | tr -d '"'\''[:space:]')"
  if [ -z "$cff" ]; then
    bad "CITATION.cff has no version"
  elif [ "$cff" != "$want_product" ]; then
    bad "product version: CITATION.cff says $cff, $matrix pins $want_product"
  else
    note "OK: CITATION.cff and $matrix both name $cff"
  fi
else
  note "no CITATION.cff yet, skipped"
fi
if [ -f Cargo.toml ]; then
  cargo_ver="$(toml_val "[workspace.package]" version Cargo.toml)"
  if [ -z "$cargo_ver" ]; then
    bad "root Cargo.toml has no [workspace.package] version"
  elif [ "$cargo_ver" != "$want_product" ]; then
    bad "product version: root Cargo.toml says $cargo_ver, $matrix pins $want_product"
  else
    note "OK: root Cargo.toml names $cargo_ver"
  fi
else
  note "no root Cargo.toml yet, skipped its version"
fi

echo "== landing-page release (website/landing/index.html <-> CHANGELOG.md)"
if [ -f website/landing/index.html ] && [ -f CHANGELOG.md ]; then
  landing_release website/landing/index.html CHANGELOG.md
else
  note "no website/landing/index.html or CHANGELOG.md yet, skipped"
fi

book_page=website/book/src/evaluate/versions.md
echo "== book pins ($book_page <-> $matrix)"
if [ -f "$book_page" ]; then
  book_pins "$book_page" "$matrix"
else
  note "no $book_page yet, skipped"
fi

echo "== CI tool pins (.github/workflows/ci.yml <-> $matrix)"
ci=.github/workflows/ci.yml
if [ -f "$ci" ]; then
  # The version each analyzer is pinned to in the workflow: an installer
  # `tool: name@version` line, or the tag of a digest-pinned image.
  ci_tool_pin() {
    case "$1" in
    zizmor | shellcheck)
      sed -nE "s|^[[:space:]]*tool:[[:space:]]*$1@([^[:space:]]+).*|\1|p" "$ci" | sort -u
      ;;
    actionlint)
      sed -nE 's|.*rhysd/actionlint:([^@[:space:]]+)@sha256:.*|\1|p' "$ci" | sort -u
      ;;
    hadolint)
      sed -nE 's|.*hadolint/hadolint:v([^@[:space:]]+)@sha256:.*|\1|p' "$ci" | sort -u
      ;;
    kubeconform)
      sed -nE 's|.*yannh/kubeconform:v([^@[:space:]]+)@sha256:.*|\1|p' "$ci" | sort -u
      ;;
    lychee)
      sed -nE 's|^[[:space:]]*LYCHEE_VERSION:[[:space:]]*([0-9][^[:space:]]*).*|\1|p' "$ci" | sort -u
      ;;
    'kubeconform schema version')
      sed -nE 's|.*-kubernetes-version[[:space:]]+([0-9][^[:space:]]*).*|\1|p' "$ci" | sort -u
      ;;
    kubernetes-json-schema)
      sed -nE 's|.*yannh/kubernetes-json-schema/([0-9a-f]{40})/.*|\1|p' "$ci" | sort -u
      ;;
    esac
  }
  ci_tools=(zizmor actionlint shellcheck hadolint kubeconform 'kubeconform schema version' kubernetes-json-schema lychee)
  for tool in "${ci_tools[@]}"; do
    want="$(pin_of "$tool" "$matrix")"
    found="$(ci_tool_pin "$tool")"
    if [ -z "$want" ]; then
      bad "$matrix has no '$tool' row"
    elif [ -z "$found" ]; then
      # hadolint has nothing to lint until a Dockerfile exists, so its absence
      # from the workflow is a skip; the other three always run.
      if [ "$tool" = hadolint ] && ! grep -q hadolint "$ci"; then
        note "$ci runs no hadolint yet, skipped"
      else
        bad "$ci pins no $tool version"
      fi
    elif [ "$(printf '%s\n' "$found" | wc -l | tr -d '[:space:]')" != "1" ]; then
      bad "$tool: $ci pins more than one version ($(printf '%s' "$found" | tr '\n' ' '))"
    elif [ "$found" != "$want" ]; then
      bad "$tool: $ci pins $found, $matrix pins $want"
    else
      note "OK: $tool $found"
    fi
  done
else
  note "no $ci yet, skipped"
fi

release_workflows=(.github/workflows/release-build.yml .github/workflows/release-image.yml .github/workflows/fuzz.yml)
# Every version of TOOL the release and fuzz workflows install, deduplicated, so a tool
# named in both files has to carry the same pin in both.
release_tool_pins() {
  local wf
  for wf in "${release_workflows[@]}"; do
    [ -f "$wf" ] || continue
    sed -nE "s|^[[:space:]]*tool:[[:space:]]*$1@([^[:space:]]+).*|\1|p" "$wf"
  done | sort -u
}
if [ -f "${release_workflows[0]}" ] || [ -f "${release_workflows[1]}" ]; then
  for tool in cargo-auditable cargo-cyclonedx syft cargo-fuzz; do
    want="$(pin_of "$tool" "$matrix")"
    found="$(release_tool_pins "$tool")"
    if [ -z "$want" ]; then
      bad "$matrix has no '$tool' row"
    elif [ -z "$found" ]; then
      bad "the release workflows pin no $tool version"
    elif [ "$(printf '%s\n' "$found" | wc -l | tr -d '[:space:]')" != "1" ]; then
      bad "$tool: the release workflows disagree ($(printf '%s' "$found" | tr '\n' ' '))"
    elif [ "$found" != "$want" ]; then
      bad "$tool: the release workflows pin $found, $matrix pins $want"
    else
      note "OK: $tool $found"
    fi
  done
else
  note "no release-build.yml or release-image.yml yet, skipped the release tool pins"
fi

echo "== docs toolchain (.github/actions/docs-toolchain <-> $matrix)"
action=.github/actions/docs-toolchain/action.yml
if [ -f "$action" ]; then
  agreed=0
  for tool in mdbook mdbook-toc mdbook-mermaid; do
    if [ "$tool" = mdbook ]; then row=mdBook; else row="$tool"; fi
    found="$(action_default "$tool-version" "$action")"
    want="$(pin_of "$row" "$matrix")"
    if [ -z "$found" ]; then
      bad "$action has no $tool-version default"
    elif [ -z "$want" ]; then
      bad "$matrix has no '$row' row"
    elif [ "$found" != "$want" ]; then
      bad "$tool: $action installs $found, $matrix pins $want"
    else
      agreed=$((agreed + 1))
    fi
  done
  [ "$agreed" -eq 3 ] && note "OK: the three docs-toolchain pins agree"
else
  note "no $action yet, skipped"
fi

echo "== testkit images (tools/ferrofed-testkit <-> $matrix)"
harness=tools/ferrofed-testkit/src/containers.rs
if [ -f "$harness" ]; then
  # The repository, tag and digest of the PinnedImage literal named CONST,
  # composed into the one reference the matrix row carries.
  image_pin_of() {
    awk -v name="$1" '
      $0 ~ "^pub const " name ": PinnedImage = PinnedImage \\{" { inside = 1; next }
      inside {
        if ($0 ~ /^\};/) { exit }
        if (match($0, /repository: "[^"]+"/)) { repo = substr($0, RSTART + 13, RLENGTH - 14) }
        if (match($0, /tag: "[^"]+"/)) { tag = substr($0, RSTART + 6, RLENGTH - 7) }
        if (match($0, /digest: "[^"]+"/)) { digest = substr($0, RSTART + 9, RLENGTH - 10) }
      }
      END { if (repo != "" && tag != "" && digest != "") print repo ":" tag "@" digest }
    ' "$2"
  }

  agreed=0
  expected=0
  for image in \
    "FerroEHR node image|FERROEHR" \
    "FerroEHR node database image|FERROEHR_POSTGRES"; do
    item="${image%%|*}"
    constant="${image##*|}"
    expected=$((expected + 1))
    want="$(pin_of "$item" "$matrix")"
    found="$(image_pin_of "$constant" "$harness")"
    if [ -z "$want" ]; then
      bad "$matrix has no '$item' row"
    elif [ -z "$found" ]; then
      bad "$harness has no $constant PinnedImage with a repository, tag and digest"
    elif [ "$found" != "$want" ]; then
      bad "$item: $harness pins $found, $matrix pins $want"
    else
      agreed=$((agreed + 1))
    fi
  done
  [ "$agreed" -eq "$expected" ] && note "OK: all $expected container image pins agree"
else
  note "no $harness yet, skipped"
fi

echo "== FHIR model crate (docs/architecture.md <-> $matrix <-> Cargo.toml)"
want="$(pin_of fhir-types "$matrix")"
if [ -z "$want" ]; then
  bad "$matrix has no fhir-types row"
else
  if [ -f docs/architecture.md ]; then
    arch="$(pin_of fhir-types docs/architecture.md)"
    if [ -z "$arch" ]; then
      bad "docs/architecture.md has no fhir-types row"
    elif [ "$arch" != "$want" ]; then
      bad "fhir-types: docs/architecture.md says $arch, $matrix pins $want"
    fi
  fi
  if [ -f Cargo.toml ]; then
    req="$(manifest_req fhir-types)"
    if [ -z "$req" ]; then
      note "root Cargo.toml has no fhir-types requirement yet, skipped"
    elif [ "$req" != "$want" ]; then
      bad "fhir-types: root Cargo.toml requires $req, $matrix pins $want"
    else
      note "OK: fhir-types $want (docs/architecture.md and the root Cargo.toml agree)"
    fi
  fi
fi

echo "== metrics crates ($matrix <-> Cargo.toml)"
# The opentelemetry crates are released in lockstep, so their rows are one
# group, as the openehr-* family's are; prometheus is held to its own row.
otel_pin=""
for crate in opentelemetry opentelemetry_sdk opentelemetry-prometheus opentelemetry-otlp prometheus; do
  want="$(pin_of "$crate" "$matrix")"
  if [ -z "$want" ]; then
    bad "$matrix has no $crate row"
    continue
  fi
  if [ "$crate" != prometheus ]; then
    if [ -z "$otel_pin" ]; then
      otel_pin="$want"
    elif [ "$want" != "$otel_pin" ]; then
      bad "$crate: $matrix pins $want, the rest of the opentelemetry group $otel_pin; the group moves together"
    fi
  fi
  if [ -f Cargo.toml ]; then
    req="$(manifest_req "$crate")"
    if [ -z "$req" ]; then
      bad "$crate: root Cargo.toml has no requirement, $matrix pins $want"
    elif [ "$req" != "$want" ]; then
      bad "$crate: root Cargo.toml requires $req, $matrix pins $want"
    else
      note "OK: $crate $want (root Cargo.toml agrees)"
    fi
  fi
done

echo "== vendored corpora (docs/specs/*/PROVENANCE.md <-> $matrix)"
# The reference a pin cell names: its first 40-hex token (a commit), else its
# first 64-hex token (the sha256 of a FHIR package tarball), else the token
# after the word `tag`.
pinned_ref_of() {
  awk '{
    for (i = 1; i <= NF; i++) if ($i ~ /^[0-9a-f]{40}$/) { print $i; exit }
    for (i = 1; i <= NF; i++) { t = $i; gsub(/[,.;:]+$/, "", t); if (t ~ /^[0-9a-f]{64}$/) { print t; exit } }
    for (i = 1; i < NF; i++) if ($i == "tag") { t = $(i + 1); gsub(/[,.;:]+$/, "", t); print t; exit }
  }' <<< "$1"
}

corpora="docs/specs/federation-spec|Federation Tier with AQL specification
docs/specs/federation-ref|Federation Tier reference implementation
docs/specs/its-rest|openEHR ITS-REST OpenAPI
docs/specs/aql|openEHR AQL specification source
docs/specs/ihe-pixm|IHE PIXm FHIR package
docs/specs/ihe-pdqm|IHE PDQm FHIR package
docs/specs/ihe-mcsd|IHE mCSD FHIR package"

agreed=0
expected=0
while IFS='|' read -r dir item; do
  [ -n "$dir" ] || continue
  expected=$((expected + 1))
  cell="$(pin_cell_of "$item" "$matrix")"
  want="$(pinned_ref_of "$cell")"
  if [ -z "$cell" ]; then
    bad "$matrix has no '$item' row"
  elif [ -z "$want" ]; then
    bad "the $matrix pin for '$item' names no commit and no tag"
  elif [ ! -f "$dir/PROVENANCE.md" ]; then
    bad "$dir/PROVENANCE.md is missing; run the vendor script that $matrix names for '$item'"
  elif ! grep -qF "$want" "$dir/PROVENANCE.md"; then
    bad "$dir/PROVENANCE.md does not name the pin $want that $matrix records for '$item'"
  else
    agreed=$((agreed + 1))
  fi
done <<< "$corpora"
[ "$agreed" -eq "$expected" ] && note "OK: all $expected corpus provenance stamps name their pin"

# The specification row pins a version and the corpus row a commit; the
# provenance records the version that commit's antora.yml declares, so a re-pin
# that moves one and not the other is caught here.
spec_prov=docs/specs/federation-spec/PROVENANCE.md
if [ -f "$spec_prov" ]; then
  want="$(pin_of "Federation Tier with AQL" "$matrix")"
  found="$(sed -nE "s/.*spec-version: '([^']+)'.*/\1/p" "$spec_prov" | head -n1)"
  if [ -z "$found" ]; then
    bad "$spec_prov records no spec-version"
  elif [ "$found" != "$want" ]; then
    bad "Federation Tier with AQL: the vendored source declares $found, $matrix pins $want"
  else
    note "OK: the vendored federation specification declares $found"
  fi
fi

echo "== container images (docker/Dockerfile, compose.yaml <-> $matrix)"
if [ -f docker/Dockerfile ]; then
  base="$(sed -nE 's|^FROM[[:space:]]+([^[:space:]]+).*|\1|p' docker/Dockerfile | head -n1)"
  want_base="$(pin_of "Container base image" "$matrix")"
  if [ -z "$base" ]; then
    bad "docker/Dockerfile has no FROM"
  elif [ -z "$want_base" ]; then
    bad "$matrix has no 'Container base image' row"
  elif [ "$base" != "$want_base" ]; then
    bad "base image: docker/Dockerfile builds on $base, $matrix pins $want_base"
  else
    note "OK: docker/Dockerfile builds on the pinned base"
  fi
  # The digest belongs to the FROM alone; the base.name label names the tag it
  # was resolved from, and the two must name the same image.
  label_base="$(sed -nE 's|.*org\.opencontainers\.image\.base\.name="([^"]+)".*|\1|p' docker/Dockerfile | head -n1)"
  if [ -n "$base" ] && [ "${base%@*}" != "$label_base" ]; then
    bad "docker/Dockerfile labels its base as '$label_base' but builds on ${base%@*}"
  fi
else
  note "no docker/Dockerfile yet, skipped"
fi
if [ -f compose.yaml ]; then
  # Every digest-pinned image is one of the matrix's pin cells, verbatim.
  pinned="$(sed -nE 's|^[[:space:]]*image:[[:space:]]*([^[:space:]]+@sha256:[0-9a-f]{64})[[:space:]]*$|\1|p' compose.yaml | sort -u)"
  agreed=0
  while IFS= read -r ref; do
    [ -n "$ref" ] || continue
    if grep -qF "\`$ref\`" "$matrix"; then
      agreed=$((agreed + 1))
    else
      bad "compose.yaml runs $ref, which no $matrix row pins"
    fi
  done <<< "$pinned"
  [ "$agreed" -gt 0 ] && note "OK: all $agreed digest-pinned compose.yaml images are rows of $matrix"
  # An image that is neither digest-pinned nor the gateway's own is a drift
  # the line above cannot see.
  while IFS= read -r ref; do
    [ -n "$ref" ] || continue
    case "$ref" in
    *@sha256:* | ghcr.io/ferrohealth/ferrofed:*) ;;
    *) bad "compose.yaml runs $ref, which is not pinned by digest" ;;
    esac
  done < <(sed -nE 's|^[[:space:]]*image:[[:space:]]*([^[:space:]]+)[[:space:]]*$|\1|p' compose.yaml)
  tags="$(sed -nE 's|^[[:space:]]*image:[[:space:]]*ghcr\.io/ferrohealth/ferrofed:\$\{[A-Za-z_][A-Za-z0-9_]*:-([^}]+)\}[[:space:]]*$|\1|p' compose.yaml | sort -u)"
  if [ -z "$tags" ]; then
    bad "compose.yaml has no ghcr.io/ferrohealth/ferrofed image tag default"
  elif [ "$(printf '%s\n' "$tags" | wc -l | tr -d '[:space:]')" -gt 1 ]; then
    bad "compose.yaml names more than one ferrofed tag default: $(printf '%s' "$tags" | tr '\n' ' ')"
  elif [ "$tags" != "$want_product" ]; then
    bad "quickstart tag: compose.yaml runs $tags, $matrix pins the product version $want_product"
  else
    note "OK: the compose.yaml gateway tag is the product version $tags"
  fi
else
  note "no compose.yaml yet, skipped"
fi
# The compose.yaml every release carries runs the gateway image alone, at the
# version of the release, so its one tag default moves with the cut.
release_compose=deploy/compose/compose.yaml
if [ -f "$release_compose" ]; then
  images="$(sed -nE 's|^[[:space:]]*image:[[:space:]]*([^[:space:]]+)[[:space:]]*$|\1|p' "$release_compose" | sort -u)"
  [ -n "$images" ] || bad "$release_compose runs no image"
  while IFS= read -r ref; do
    [ -n "$ref" ] || continue
    case "$ref" in
    "ghcr.io/ferrohealth/ferrofed:\${FERROFED_VERSION:-"*"}") ;;
    *) bad "$release_compose runs $ref, which is not ghcr.io/ferrohealth/ferrofed:\${FERROFED_VERSION:-<version>}" ;;
    esac
  done <<< "$images"
  tags="$(sed -nE 's|^[[:space:]]*image:[[:space:]]*ghcr\.io/ferrohealth/ferrofed:\$\{FERROFED_VERSION:-([^}]+)\}[[:space:]]*$|\1|p' "$release_compose" | sort -u)"
  if [ -z "$tags" ]; then
    bad "$release_compose has no ghcr.io/ferrohealth/ferrofed image tag default"
  elif [ "$(printf '%s\n' "$tags" | wc -l | tr -d '[:space:]')" -gt 1 ]; then
    bad "$release_compose names more than one ferrofed tag default: $(printf '%s' "$tags" | tr '\n' ' ')"
  elif [ "$tags" != "$want_product" ]; then
    bad "release compose tag: $release_compose runs $tags, $matrix pins the product version $want_product"
  else
    note "OK: the $release_compose gateway tag is the product version $tags"
  fi
else
  note "no $release_compose yet, skipped"
fi
deployment=deploy/kubernetes/deployment.yaml
if [ -f "$deployment" ]; then
  tags="$(sed -nE 's|^[[:space:]]*image:[[:space:]]*ghcr\.io/ferrohealth/ferrofed:([^@[:space:]]+)[[:space:]]*$|\1|p' "$deployment" | sort -u)"
  if [ -z "$tags" ]; then
    bad "$deployment has no ghcr.io/ferrohealth/ferrofed image tag"
  elif [ "$(printf '%s\n' "$tags" | wc -l | tr -d '[:space:]')" -gt 1 ]; then
    bad "$deployment names more than one ferrofed tag: $(printf '%s' "$tags" | tr '\n' ' ')"
  elif [ "$tags" != "$want_product" ]; then
    bad "example manifest: $deployment runs $tags, $matrix pins the product version $want_product"
  else
    note "OK: the $deployment gateway tag is the product version $tags"
  fi
else
  note "no $deployment yet, skipped"
fi

echo "== licence (LICENSE <-> SPDX headers, manifests, badges, labels)"
if [ -f LICENSE ]; then
  stale=0
  if ! grep -q 'Business Source License 1.1' LICENSE; then
    bad "LICENSE is not the Business Source License 1.1"
    stale=1
  fi
  # The SPDX tag is anchored to the start of its line, after an optional
  # comment marker, so a header claim is caught while the same text quoted
  # inside a string literal is not. Vendored trees keep their upstream terms
  # and are outside the check.
  while IFS= read -r hit; do
    [ -n "$hit" ] || continue
    bad "stale licence claim at $hit"
    stale=1
  done < <(git grep -n -E '^[[:space:]]*([/#*]+|<!--)?[[:space:]]*SPDX-License-Identifier: (MIT|Apache-2\.0)|License-MIT|License-Apache|^license = "(MIT|Apache-2\.0)"|^license: (MIT|Apache-2\.0)|image\.licenses="?(MIT|Apache)' \
    -- ':!LICENSE' ':!CHANGELOG.md' ':!scripts/checks/versions.sh' ':(glob,exclude)**/vendor/**' \
    ':(glob,exclude)docs/specs/**' || true)
  [ "$stale" -eq 0 ] && note "OK: every first-party file names BUSL-1.1"
else
  bad "LICENSE is missing"
fi

echo
if [ "$fail" -ne 0 ]; then
  echo "versions: DRIFT detected" >&2
  exit 1
fi
echo "versions: OK (every present check agrees)"
