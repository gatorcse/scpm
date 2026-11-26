//! Maven version ordering and npm-style ranges with Scala's early-semver rules (design §5.5).

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::fmt;

/// A version compared the way Maven's `ComparableVersion` does (a total order): `.` and `-`
/// separate items and each `-` (or a switch between digits and letters) opens a nested list;
/// numbers compare numerically, qualifiers by rank (`alpha < beta < milestone < rc < snapshot <
/// release < sp < other`, others lexically), with `a`/`b`/`m` before a digit and `cr` as
/// aliases. At one position, a number beats a nested list, which beats a qualifier.
#[derive(Debug, Clone)]
pub struct MavenVersion {
    raw: String,
    items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    /// Decimal digits without leading zeros (`0` is `"0"`), compared by length then text.
    Int(String),
    /// A lower-cased qualifier with Maven's aliases applied (`ga`/`final`/`release` → ``).
    Str(String),
    List(Vec<Item>),
}

impl Item {
    fn int(digits: &str) -> Item {
        let trimmed = digits.trim_start_matches('0');
        Item::Int(if trimmed.is_empty() { "0".to_string() } else { trimmed.to_string() })
    }

    fn string(value: &str, followed_by_digit: bool) -> Item {
        let value = match value {
            "a" if followed_by_digit => "alpha",
            "b" if followed_by_digit => "beta",
            "m" if followed_by_digit => "milestone",
            "ga" | "final" | "release" => "",
            "cr" => "rc",
            other => other,
        };
        Item::Str(value.to_string())
    }

    fn parse(is_digit: bool, text: &str) -> Item {
        if is_digit { Item::int(text) } else { Item::string(text, false) }
    }

    /// `0`, the release qualifier and an empty list are no-ops at the end of a list.
    fn is_null(&self) -> bool {
        match self {
            Item::Int(n) => n == "0",
            Item::Str(s) => s.is_empty(),
            Item::List(l) => l.is_empty(),
        }
    }

    /// How the item compares with an absent one (a release with nothing more).
    fn cmp_absent(&self) -> Ordering {
        match self {
            Item::Int(n) => {
                if n == "0" {
                    Ordering::Equal
                } else {
                    Ordering::Greater
                }
            }
            Item::Str(s) => comparable_qualifier(s).cmp(&RELEASE_RANK.to_string()),
            Item::List(l) => l.iter().map(Item::cmp_absent).find(|o| o.is_ne()).unwrap_or(Ordering::Equal),
        }
    }
}

fn cmp_item(a: &Item, b: &Item) -> Ordering {
    match (a, b) {
        (Item::Int(x), Item::Int(y)) => x.len().cmp(&y.len()).then_with(|| x.cmp(y)),
        (Item::Int(_), _) => Ordering::Greater,
        (_, Item::Int(_)) => Ordering::Less,
        (Item::Str(x), Item::Str(y)) => comparable_qualifier(x).cmp(&comparable_qualifier(y)),
        (Item::Str(_), Item::List(_)) => Ordering::Less,
        (Item::List(_), Item::Str(_)) => Ordering::Greater,
        (Item::List(x), Item::List(y)) => cmp_lists(x, y),
    }
}

fn cmp_lists(a: &[Item], b: &[Item]) -> Ordering {
    for i in 0..a.len().max(b.len()) {
        let ord = match (a.get(i), b.get(i)) {
            (Some(x), Some(y)) => cmp_item(x, y),
            (Some(x), None) => x.cmp_absent(),
            (None, Some(y)) => y.cmp_absent().reverse(),
            (None, None) => Ordering::Equal,
        };
        if ord.is_ne() {
            return ord;
        }
    }
    Ordering::Equal
}

/// Maven's sort key for a qualifier: its rank as a digit, or `7-<qualifier>` for unknown ones.
fn comparable_qualifier(q: &str) -> String {
    const ORDER: [&str; 7] = ["alpha", "beta", "milestone", "rc", "snapshot", "", "sp"];
    match ORDER.iter().position(|o| *o == q) {
        Some(i) => i.to_string(),
        None => format!("{}-{q}", ORDER.len()),
    }
}

/// Drops trailing no-op items (`1.0.0` → `1`, `1-final` → `1`), also before a nested list.
fn normalize(list: &mut Vec<Item>) {
    let mut i = list.len();
    while i > 0 {
        i -= 1;
        if list[i].is_null() {
            list.remove(i);
        } else if !matches!(list[i], Item::List(_)) {
            break;
        }
    }
}

const RELEASE_RANK: u8 = 5;

fn qualifier_rank(q: &str) -> u8 {
    match q {
        "alpha" | "a" => 0,
        "beta" | "b" => 1,
        "milestone" | "m" => 2,
        "rc" | "cr" => 3,
        "snapshot" => 4,
        "" | "ga" | "final" | "release" => RELEASE_RANK,
        "sp" => 6,
        _ => 7,
    }
}

/// Qualifiers that mark a pre-release (beyond Maven's ranks, which treat these as "other").
fn is_prerelease_qualifier(q: &str) -> bool {
    qualifier_rank(q) < RELEASE_RANK || matches!(q, "pre" | "preview" | "ea" | "dev" | "next" | "nightly")
}

impl MavenVersion {
    /// Parses like Maven's `ComparableVersion.parseVersion`.
    pub fn parse(raw: &str) -> MavenVersion {
        let version = raw.to_ascii_lowercase();
        // The innermost open list is last; each nested list ends up as its parent's last item.
        let mut stack: Vec<Vec<Item>> = vec![Vec::new()];
        let mut is_digit = false;
        let mut start = 0;
        for (i, c) in version.char_indices() {
            let list = stack.last_mut().expect("the root list");
            match c {
                '.' | '-' => {
                    list.push(if i == start { Item::int("0") } else { Item::parse(is_digit, &version[start..i]) });
                    start = i + 1;
                    if c == '-' {
                        stack.push(Vec::new());
                    }
                }
                c if c.is_ascii_digit() => {
                    if !is_digit && i > start {
                        list.push(Item::string(&version[start..i], true));
                        start = i;
                        stack.push(Vec::new());
                    }
                    is_digit = true;
                }
                _ => {
                    if is_digit && i > start {
                        list.push(Item::int(&version[start..i]));
                        start = i;
                        stack.push(Vec::new());
                    }
                    is_digit = false;
                }
            }
        }
        if version.len() > start {
            stack.last_mut().expect("the root list").push(Item::parse(is_digit, &version[start..]));
        }
        let mut items = stack.pop().expect("the root list");
        normalize(&mut items);
        while let Some(mut parent) = stack.pop() {
            parent.push(Item::List(items));
            normalize(&mut parent);
            items = parent;
        }
        MavenVersion { raw: raw.to_string(), items }
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Pre-releases (`-RC1`, `-M3`, `-SNAPSHOT`, …) and dynver builds (`1.2.3+4-abcdef12`),
    /// which are never releases.
    pub fn is_prerelease(&self) -> bool {
        fn any_prerelease(items: &[Item]) -> bool {
            items.iter().any(|i| match i {
                Item::Str(q) => is_prerelease_qualifier(q),
                Item::List(l) => any_prerelease(l),
                Item::Int(_) => false,
            })
        }
        self.raw.contains('+') || any_prerelease(&self.items)
    }

    /// The release "flavour" a qualifier selects, e.g. `jre` in `33.4.0-jre`, `android` in
    /// `33.0.0-android`, `jre7` in `42.2.29.jre7`; `None` for plain versions. Pre-release,
    /// release (`Final`, `GA`) and service-pack qualifiers are not flavours.
    pub fn flavour(&self) -> Option<String> {
        let rest = self.raw.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.');
        let tokens: Vec<String> = rest
            .split(['-', '.', '_'])
            .filter(|t| !t.is_empty() && !t.chars().all(|c| c.is_ascii_digit()))
            .map(str::to_ascii_lowercase)
            .filter(|t| {
                let word = t.trim_end_matches(|c: char| c.is_ascii_digit());
                !is_prerelease_qualifier(word) && qualifier_rank(word) != RELEASE_RANK && word != "sp"
            })
            .collect();
        (!tokens.is_empty()).then(|| tokens.join("-"))
    }

    /// The first three numeric components, padding with zeros: `1.2` → `[1, 2, 0]`. `None` if
    /// one does not fit a `u128`.
    fn numeric_prefix(&self) -> Option<[u128; 3]> {
        let mut out = [0; 3];
        for (slot, item) in out.iter_mut().zip(&self.items) {
            match item {
                Item::Int(n) => *slot = n.parse().ok()?,
                _ => break,
            }
        }
        Some(out)
    }
}

impl fmt::Display for MavenVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl PartialEq for MavenVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for MavenVersion {}

impl PartialOrd for MavenVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MavenVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        cmp_lists(&self.items, &other.items)
    }
}

/// A version range: a conjunction of comparators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Range {
    raw: String,
    bounds: Vec<(Op, MavenVersion)>,
    /// Whether the spec names a pre-release, which lets pre-releases match.
    allows_prerelease: bool,
    /// The qualifier flavour the bounds name (`android` in `^33.0.0-android`): only versions
    /// with the same flavour match.
    flavour: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Ge,
    Gt,
    Le,
    Lt,
    Eq,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid version range `{range}`: {reason}")]
pub struct RangeError {
    pub range: String,
    pub reason: String,
}

impl Range {
    /// Parses `^1.2.3`, `~1.2.3`, `1.2.x`, `*`, or space-separated comparators such as
    /// `>=1.2 <2`. Bounds follow early-semver: `^0.14.1` → `>=0.14.1 <0.15.0`.
    pub fn parse(raw: &str) -> Result<Range, RangeError> {
        let err = |reason: String| RangeError { range: raw.to_string(), reason };
        let s = raw.trim();
        if s.is_empty() {
            return Err(err("the range is empty".into()));
        }
        if s.starts_with('[') || s.starts_with('(') {
            return Err(err("Maven interval syntax is not supported in manifests; use `>=x <y`, `^x` or `~x`".into()));
        }
        if s.contains("||") {
            return Err(err("unions are not supported; use a single range such as `>=1.2 <3`".into()));
        }
        // A bound must be a version: it starts with a digit (`1.2`, `33.0.0-android`).
        let version = |v: &str| -> Result<MavenVersion, RangeError> {
            if v.is_empty() {
                return Err(err("missing version after the operator".into()));
            }
            if !v.starts_with(|c: char| c.is_ascii_digit()) {
                return Err(err(format!("`{v}` is not a version")));
            }
            if v.ends_with('+') {
                return Err(err(format!(
                    "Ivy dynamic versions (`{v}`) are not supported; use a wildcard such as `1.2.x`"
                )));
            }
            Ok(MavenVersion::parse(v))
        };
        let too_big = |v: &str| err(format!("`{v}` is too large to compute an upper bound"));
        let prefix = |low: &MavenVersion, v: &str| low.numeric_prefix().ok_or_else(|| too_big(v));
        let next = |n: u128, v: &str| n.checked_add(1).ok_or_else(|| too_big(v));
        let mut bounds = Vec::new();
        for part in s.split_whitespace() {
            if part == "*" || part == "x" {
                continue;
            }
            let op_len = part.find(|c: char| c.is_ascii_alphanumeric()).unwrap_or(part.len());
            let (op, v) = part.split_at(op_len);
            match op {
                "^" => {
                    let low = version(v)?;
                    let [major, minor, _] = prefix(&low, v)?;
                    let high =
                        if major > 0 { format!("{}.0.0", next(major, v)?) } else { format!("0.{}.0", next(minor, v)?) };
                    bounds.push((Op::Ge, low));
                    bounds.push((Op::Lt, MavenVersion::parse(&high)));
                }
                "~" => {
                    let low = version(v)?;
                    let [major, minor, _] = prefix(&low, v)?;
                    bounds.push((Op::Ge, low));
                    bounds.push((Op::Lt, MavenVersion::parse(&format!("{major}.{}.0", next(minor, v)?))));
                }
                "" if v.ends_with(".x") || v.ends_with(".*") => {
                    let prefix = &v[..v.len() - 2];
                    let bad = || err(format!("`{v}` is not a valid wildcard"));
                    let parts: Vec<u128> =
                        prefix.split('.').map(|p| p.parse().map_err(|_| bad())).collect::<Result<_, _>>()?;
                    let mut high = parts.clone();
                    let last = high.last_mut().ok_or_else(bad)?;
                    *last = next(*last, v)?;
                    let high: Vec<String> = high.iter().map(u128::to_string).collect();
                    bounds.push((Op::Ge, MavenVersion::parse(prefix)));
                    bounds.push((Op::Lt, MavenVersion::parse(&high.join("."))));
                }
                ">=" | "<=" | ">" | "<" | "=" | "" => {
                    let op = match op {
                        ">=" => Op::Ge,
                        "<=" => Op::Le,
                        ">" => Op::Gt,
                        "<" => Op::Lt,
                        _ => Op::Eq,
                    };
                    bounds.push((op, version(v)?));
                }
                other => {
                    return Err(err(format!(
                        "unknown operator `{other}`; use `^`, `~`, `>=`, `>`, `<=`, `<`, `=` or a wildcard (`1.2.x`)"
                    )));
                }
            }
        }
        let allows_prerelease = bounds.iter().any(|(_, v)| v.is_prerelease());
        let flavours: BTreeSet<String> = bounds.iter().filter_map(|(_, v)| v.flavour()).collect();
        if flavours.len() > 1 {
            return Err(err(format!(
                "the bounds name different qualifiers ({})",
                flavours.into_iter().collect::<Vec<_>>().join(", ")
            )));
        }
        Ok(Range { raw: raw.to_string(), bounds, allows_prerelease, flavour: flavours.into_iter().next() })
    }

    pub fn matches(&self, v: &MavenVersion) -> bool {
        if v.is_prerelease() && !self.allows_prerelease {
            return false;
        }
        if self.flavour.is_some() && v.flavour() != self.flavour {
            return false;
        }
        self.bounds.iter().all(|(op, bound)| match op {
            Op::Ge => v >= bound,
            Op::Gt => v > bound,
            Op::Le => v <= bound,
            Op::Lt => v < bound,
            Op::Eq => v == bound,
        })
    }

    /// The highest matching version among `available`. When the bounds name no qualifier,
    /// plain versions win over flavoured ones (`42.2.29` over `42.2.29.jre7`).
    pub fn select<'a>(&self, available: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
        let matching: Vec<(MavenVersion, &str)> =
            available.into_iter().map(|s| (MavenVersion::parse(s), s)).filter(|(v, _)| self.matches(v)).collect();
        let highest = |candidates: Vec<&(MavenVersion, &'a str)>| {
            candidates.into_iter().max_by(|a, b| a.0.cmp(&b.0)).map(|(_, s)| *s)
        };
        if self.flavour.is_none()
            && let Some(plain) = highest(matching.iter().filter(|(v, _)| v.flavour().is_none()).collect())
        {
            return Some(plain);
        }
        highest(matching.iter().collect())
    }
}

/// The highest release (not a pre-release or dynver build) among `available`.
pub fn latest_release<'a>(available: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    available
        .into_iter()
        .map(|s| (MavenVersion::parse(s), s))
        .filter(|(v, _)| !v.is_prerelease())
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, s)| s)
}

impl fmt::Display for Range {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> MavenVersion {
        MavenVersion::parse(s)
    }

    #[test]
    fn maven_ordering() {
        let ordered = [
            "1.0-alpha1",
            "1.0-beta2",
            "1.0-M1",
            "1.0-RC1",
            "1.0-RC2",
            "1.0-SNAPSHOT",
            "1.0",
            "1.0-sp1",
            "1.0-jre",
            "1.0.1",
            "1.1",
            "1.10",
            "2.0.0-RC1",
            "2.0.0",
        ];
        for w in ordered.windows(2) {
            assert!(v(w[0]) < v(w[1]), "{} < {}", w[0], w[1]);
        }
        assert_eq!(v("1.0"), v("1.0.0"));
        assert_eq!(v("1.0"), v("1.0-final"));
    }

    #[test]
    fn ordering_is_total() {
        // `1 < 1-sp`, `1.0-rc1 < 1` and `1-sp > 1.0-rc1` used to form a cycle.
        let corpus = [
            "1",
            "1-sp",
            "1.0-rc1",
            "1.0-RC2",
            "1.0-cr2",
            "1.0.0-RC1",
            "1.0-jre",
            "1.0.1",
            "1-1",
            "1.0-1",
            "1-sp1",
            "1.0-alpha",
            "1.0-a1",
            "1-b2",
            "1.0-M1",
            "1.0-SNAPSHOT",
            "1.0-final",
            "1.0.0",
            "1.1",
            "1.0-foo",
            "1.0-bar",
            "1.0.0.1",
            "1.0-0",
            "2",
            "2.0-rc1",
            "0.9",
            "1-ga",
            "1.0-sp-1",
            "1.0rc1",
            "1.0.jre7",
            "1.2.3+4-abc",
        ];
        let vs: Vec<MavenVersion> = corpus.iter().map(|s| v(s)).collect();
        for a in &vs {
            for b in &vs {
                assert_eq!(a.cmp(b), b.cmp(a).reverse(), "{a} vs {b}");
                for c in &vs {
                    if a <= b && b <= c {
                        assert!(a <= c, "{a} <= {b} <= {c}");
                    }
                }
            }
        }
        // The highest version doesn't depend on the listing order.
        let range = Range::parse("<2.0-rc1").unwrap();
        for order in [["1", "1-sp", "1.0-rc1"], ["1.0-rc1", "1-sp", "1"], ["1-sp", "1", "1.0-rc1"]] {
            assert_eq!(range.select(order), Some("1-sp"), "{order:?}");
        }
    }

    #[test]
    fn qualifiers_follow_maven() {
        assert!(v("1.0-RC2") > v("1.0.0-RC1"));
        assert!(Range::parse(">=1.0.0-RC1").unwrap().matches(&v("1.0-RC2")));
        assert_eq!(v("1.0-cr1"), v("1.0-rc1"), "cr is an alias of rc");
        assert!(Range::parse(">=1.0-rc1").unwrap().matches(&v("1.0-cr2")));
        assert!(Range::parse(">=1.0-rc1").unwrap().matches(&v("1.0-cr1")));
        assert_eq!(v("1.0-a1"), v("1.0-alpha1"));
        assert!(v("1.0-foo") < v("1.0-goo"), "unknown qualifiers sort lexically");
    }

    #[test]
    fn upper_bounds_that_overflow_are_errors() {
        let max = u128::MAX;
        for r in [format!("^{max}.0.0"), format!("^0.{max}.0"), format!("~1.{max}"), format!("1.{max}.x")] {
            assert!(Range::parse(&r).unwrap_err().reason.contains("too large"), "{r}");
        }
        assert!(Range::parse(&format!("^{max}0.0")).unwrap_err().reason.contains("too large"));
        assert!(Range::parse("^1.2.3").is_ok());
    }

    #[test]
    fn ivy_selectors_are_rejected() {
        assert!(Range::parse("1.2.+").unwrap_err().reason.contains("Ivy"));
        assert!(Range::parse(">=1.2.+").unwrap_err().reason.contains("Ivy"));
        assert!(Range::parse("1.2.x").is_ok() && Range::parse("1.2.3+4-abcdef12").is_ok());
    }

    #[test]
    fn prereleases() {
        for p in ["2.0.0-RC1", "1.0-M3", "3.0.0-alpha.1", "1.0-SNAPSHOT", "0.5-preview"] {
            assert!(v(p).is_prerelease(), "{p}");
        }
        for r in ["2.0.0", "33.4.0-jre", "4.1.115.Final", "1.0-sp1"] {
            assert!(!v(r).is_prerelease(), "{r}");
        }
    }

    #[test]
    fn early_semver_caret_and_tilde() {
        let caret = Range::parse("^1.2.3").unwrap();
        assert!(caret.matches(&v("1.9.0")) && !caret.matches(&v("2.0.0")) && !caret.matches(&v("1.2.2")));
        assert!(!caret.matches(&v("2.0.0-RC1")), "pre-releases of the next major are excluded");
        let zero = Range::parse("^0.14.10").unwrap();
        assert!(zero.matches(&v("0.14.14")) && !zero.matches(&v("0.15.0")));
        let tilde = Range::parse("~1.2.3").unwrap();
        assert!(tilde.matches(&v("1.2.9")) && !tilde.matches(&v("1.3.0")));
    }

    #[test]
    fn comparators_and_wildcards() {
        let r = Range::parse(">=1.2 <2").unwrap();
        assert!(r.matches(&v("1.2")) && r.matches(&v("1.99")) && !r.matches(&v("2.0")));
        let w = Range::parse("1.2.x").unwrap();
        assert!(w.matches(&v("1.2.7")) && !w.matches(&v("1.3.0")));
        assert!(Range::parse("*").unwrap().matches(&v("9.9")));
        assert!(Range::parse("[1.0,2.0)").is_err());
    }

    #[test]
    fn prerelease_specs_allow_prereleases() {
        let r = Range::parse(">=2.0.0-RC1").unwrap();
        assert!(r.matches(&v("2.0.0-RC2")));
    }

    #[test]
    fn strict_parsing() {
        let reason = |r: &str| Range::parse(r).unwrap_err().reason;
        assert!(reason("").contains("empty"));
        assert!(reason("   ").contains("empty"));
        assert!(reason(">=").contains("missing version"));
        assert!(reason("^").contains("missing version"));
        assert!(reason(">=abc").contains("`abc` is not a version"));
        assert!(reason("^v1.2").contains("`v1.2` is not a version"));
        assert!(reason("latest").contains("not a version"));
        assert!(reason("=>1.0").contains("unknown operator `=>`"));
        assert!(reason("~>1.0").contains("unknown operator `~>`"));
        assert!(reason("!=1.0").contains("unknown operator `!=`"));
        assert!(reason("1.0 - 2.0").contains("unknown operator `-`"));
        assert!(reason("^1.0 || ^2.0").contains("unions are not supported"));
        assert!(reason("1.0||2.0").contains("unions are not supported"));
        assert!(reason("a.x").contains("not a valid wildcard"));
        assert!(reason(">=1.0-jre <2.0-android").contains("different qualifiers"));
        assert!(Range::parse("=1.2.3").is_ok() && Range::parse("1.2.*").is_ok());
    }

    #[test]
    fn flavours() {
        assert_eq!(v("33.4.0-jre").flavour().as_deref(), Some("jre"));
        assert_eq!(v("33.0.0-android").flavour().as_deref(), Some("android"));
        assert_eq!(v("42.2.29.jre7").flavour().as_deref(), Some("jre7"));
        for plain in ["42.2.29", "4.1.115.Final", "1.0-sp1", "2.0.0-RC1", "3.0.0-alpha.1", "1.0-GA"] {
            assert_eq!(v(plain).flavour(), None, "{plain}");
        }
    }

    #[test]
    fn qualified_bounds_only_match_their_flavour() {
        let available = ["32.1.3-android", "32.1.3-jre", "33.0.0-android", "33.4.0-android", "33.4.0-jre"];
        assert_eq!(Range::parse("^33.0.0-android").unwrap().select(available), Some("33.4.0-android"));
        assert_eq!(Range::parse("^32.0.0-jre").unwrap().select(available), Some("32.1.3-jre"));
        assert!(!Range::parse("^33.0.0-android").unwrap().matches(&v("33.4.0-jre")));
    }

    #[test]
    fn unqualified_bounds_prefer_plain_versions() {
        let available = ["42.2.28", "42.2.28.jre7", "42.2.29", "42.2.29.jre7", "42.2.29.jre6"];
        assert_eq!(Range::parse("~42.2.0").unwrap().select(available), Some("42.2.29"));
        // With only flavoured versions in range, the highest one is still found.
        assert_eq!(Range::parse("^33.0").unwrap().select(["33.4.0-android", "33.4.0-jre"]), Some("33.4.0-jre"));
    }

    #[test]
    fn dynver_builds_are_not_releases() {
        assert!(v("1.2.3+4-abcdef12").is_prerelease());
        assert!(v("0.1.0+3-1a2b3c4d-SNAPSHOT").is_prerelease());
        assert_eq!(Range::parse("^1.2.0").unwrap().select(["1.2.3", "1.2.3+4-abcdef12"]), Some("1.2.3"));
    }

    #[test]
    fn latest_release_skips_prereleases() {
        assert_eq!(latest_release(["1.0.0", "2.0.0-RC1", "1.1.0", "1.2.0+3-deadbeef"]), Some("1.1.0"));
        assert_eq!(latest_release(["2.0.0-M1"]), None);
    }

    #[test]
    fn selects_highest_match() {
        let available = ["0.14.9", "0.14.10", "0.14.14", "0.15.0-M1", "0.15.0"];
        assert_eq!(Range::parse("^0.14.10").unwrap().select(available), Some("0.14.14"));
        assert_eq!(Range::parse("^0.16").unwrap().select(available), None);
    }
}
