//! Keeping the order of the user's language list, without overwriting
//! anything else.
//!
//! The order is stored in the REG_MULTI_SZ value `Languages` under
//! `HKCU\Control Panel\International\User Profile`; without a display-language
//! override (`WindowsOverride`), Windows uses the first entry as the display
//! language. Each language has a subkey there whose values include its input
//! methods (`"0409:00000409"`, a DWORD). `InstallLayoutOrTip` rewrites this
//! configuration, and adding and removing layouts can move a language to the
//! end of the list.
//!
//! Putting the order back needs `Set-WinUserLanguageList`, which writes the
//! whole list (input methods, spelling, handwriting and more) as it read it.
//! So KeyDock restores only when it can tell that nothing but its own layout
//! change touched the list:
//!
//! 1. it snapshots the whole key before the layout change ([`Snapshot`]);
//! 2. after the change it checks that every difference is explained by the
//!    layouts it installed or removed ([`explain_change`]); anything else is a
//!    conflict and nothing is written;
//! 3. right before PowerShell starts, the key must still be exactly as in
//!    step 2; inside the script, the language list must still have the same
//!    languages, order and input methods ([`restore_script`]) immediately
//!    before `Set-WinUserLanguageList`;
//! 4. afterwards the key must equal step 2's snapshot apart from the order
//!    ([`verify_restored`]).
//!
//! Attribution is a heuristic: a reorder that looks exactly like Windows'
//! side effect (a language whose layouts changed moved to the end) cannot be
//! told apart from the user making the same move, and a change made by
//! another program in the moment between the script's read and its write can
//! still be lost (Windows offers no compare-and-swap for this list). So the
//! restore is opt-in: the default is [`OrderPolicy::Report`], which never
//! starts PowerShell. In either mode the layout changes themselves
//! (`InstallLayoutOrTip`) can still reorder the list, add a language or
//! remove one whose last layout is removed.

use std::collections::{BTreeMap, BTreeSet};

/// Splits REG_MULTI_SZ data (UTF-16, NUL-separated, double-NUL-terminated).
pub fn parse_multi_sz(data: &[u16]) -> Vec<String> {
    data.split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

/// The order to restore, or `None` when `after` already keeps the order of
/// `before`. Languages present in both lists go first, in their `before`
/// order; languages that are new in `after` follow in their `after` order;
/// languages that disappeared are not brought back (adding or removing
/// languages is not KeyDock's business, only their order).
pub fn restore_order(before: &[String], after: &[String]) -> Option<Vec<String>> {
    let mut want: Vec<String> = before
        .iter()
        .filter(|t| after.iter().any(|a| a.eq_ignore_ascii_case(t)))
        .map(|t| {
            // Use the spelling from `after`, that is what Windows has now.
            after
                .iter()
                .find(|a| a.eq_ignore_ascii_case(t))
                .cloned()
                .unwrap_or_else(|| t.to_string())
        })
        .collect();
    for a in after {
        if !want.iter().any(|w| w.eq_ignore_ascii_case(a)) {
            want.push(a.clone());
        }
    }
    (want != after).then_some(want)
}

/// Whether a language tag is safe to put into a PowerShell single-quoted
/// string (BCP 47 tags use only letters, digits and hyphens).
pub fn is_safe_tag(tag: &str) -> bool {
    !tag.is_empty() && tag.len() <= 85 && tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Whether an input-method identifier is safe to put into a PowerShell
/// single-quoted string: `0409:00000409` or `0411:{GUID}{GUID}`.
pub fn is_safe_tip(tip: &str) -> bool {
    !tip.is_empty()
        && tip.len() <= 100
        && tip
            .chars()
            .all(|c| c.is_ascii_hexdigit() || matches!(c, ':' | '{' | '}' | '-'))
}

/// A registry value: type and raw data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegValue {
    pub ty: u32,
    pub data: Vec<u8>,
}

/// The `User Profile` key: its own values and those of each language subkey.
/// Names are stored in lower case (registry names are case-insensitive).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// The `Languages` value, in order, as written.
    pub languages: Vec<String>,
    /// Values of the key itself except `Languages`.
    pub root: BTreeMap<String, RegValue>,
    /// Language subkey (lower case) -> its values.
    pub subkeys: BTreeMap<String, BTreeMap<String, RegValue>>,
}

/// An input-method value name (`0409:00000409`, `0411:{...}{...}`).
pub fn is_tip_name(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() > 5 && b[..4].iter().all(|c| c.is_ascii_hexdigit()) && b[4] == b':'
}

impl Snapshot {
    /// Input methods of a language (upper case).
    pub fn tips(&self, tag: &str) -> BTreeSet<String> {
        self.subkeys
            .get(&tag.to_ascii_lowercase())
            .map(|v| {
                v.keys()
                    .filter(|n| is_tip_name(n))
                    .map(|n| n.to_ascii_uppercase())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn has_language(&self, tag: &str) -> bool {
        self.languages.iter().any(|l| l.eq_ignore_ascii_case(tag))
    }

    /// `(tag, input methods)` in list order, for [`restore_script`].
    pub fn expected_list(&self) -> Vec<(String, Vec<String>)> {
        self.languages
            .iter()
            .map(|t| (t.clone(), self.tips(t).into_iter().collect()))
            .collect()
    }
}

/// What KeyDock's own layout change did to the language list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Change {
    /// Languages that were added to the list (their first layout was installed).
    pub added: Vec<String>,
    /// Languages that left the list (their last layout was removed).
    pub removed: Vec<String>,
    /// The first language before the change, if it left the list. Without a
    /// display-language override Windows then shows another display language
    /// at the next sign-in.
    pub first_removed: Option<String>,
}

fn values_equal_except_tip_data(a: &BTreeMap<String, RegValue>, b: &BTreeMap<String, RegValue>) -> bool {
    let non_tip = |m: &BTreeMap<String, RegValue>| {
        m.iter()
            .filter(|(n, _)| !is_tip_name(n))
            .map(|(n, v)| (n.clone(), v.clone()))
            .collect::<Vec<_>>()
    };
    non_tip(a) == non_tip(b)
}

fn upper(v: &[String]) -> BTreeSet<String> {
    v.iter().map(|s| s.to_ascii_uppercase()).collect()
}

/// Checks that every difference between `before` and `after` comes from
/// installing `installed` and removing `removed` (input-method strings such
/// as `0409:00000409`), and describes it. `Err` names the first difference
/// that something else must have made; then nothing may be restored, because
/// writing the whole list would replace that change.
///
/// Explained differences: input methods added (from `installed`) or removed
/// (from `removed`) in a language subkey, the order numbers of the remaining
/// input methods of such a language, languages that appear with only
/// installed input methods or disappear with only removed ones, and the
/// position of languages whose input methods changed, as long as they moved
/// to the end of the list. The relative order of all other languages, every
/// other value and every untouched language must be unchanged.
pub fn explain_change(
    before: &Snapshot,
    after: &Snapshot,
    installed: &[String],
    removed: &[String],
) -> Result<Change, String> {
    let installed = upper(installed);
    let removed_tips = upper(removed);
    if before.root != after.root {
        return Err("a language setting other than the order changed".into());
    }
    let mut touched: BTreeSet<String> = BTreeSet::new();
    let tags: BTreeSet<&String> = before.subkeys.keys().chain(after.subkeys.keys()).collect();
    for tag in tags {
        let (b, a) = (before.subkeys.get(tag), after.subkeys.get(tag));
        let (bt, at) = (before.tips(tag), after.tips(tag));
        let add: BTreeSet<_> = at.difference(&bt).cloned().collect();
        let del: BTreeSet<_> = bt.difference(&at).cloned().collect();
        if !add.is_subset(&installed) {
            return Err(format!("input method(s) {add:?} appeared for {tag}"));
        }
        if !del.is_subset(&removed_tips) {
            return Err(format!("input method(s) {del:?} disappeared from {tag}"));
        }
        match (b, a) {
            (Some(b), Some(a)) if add.is_empty() && del.is_empty() => {
                if b != a {
                    return Err(format!("the settings of {tag} changed"));
                }
            }
            (Some(b), Some(a)) => {
                if !values_equal_except_tip_data(b, a) {
                    return Err(format!("the settings of {tag} changed"));
                }
                touched.insert(tag.clone());
            }
            _ => {
                touched.insert(tag.clone());
            }
        }
    }
    let mut change = Change::default();
    for l in &after.languages {
        if !before.has_language(l) {
            let tips = after.tips(l);
            if tips.is_empty() || !tips.is_subset(&installed) {
                return Err(format!("language {l} was added"));
            }
            change.added.push(l.clone());
        }
    }
    for l in &before.languages {
        if !after.has_language(l) {
            let tips = before.tips(l);
            if !tips.is_subset(&removed_tips) {
                return Err(format!("language {l} was removed"));
            }
            change.removed.push(l.clone());
        }
    }
    // Languages whose input methods did not change keep their relative order.
    let untouched = |list: &[String]| -> Vec<String> {
        list.iter()
            .filter(|l| !touched.contains(&l.to_ascii_lowercase()))
            .map(|l| l.to_ascii_lowercase())
            .collect()
    };
    let (ub, ua) = (untouched(&before.languages), untouched(&after.languages));
    let common = |x: &[String], y: &[String]| x.iter().filter(|l| y.contains(l)).cloned().collect::<Vec<_>>();
    if common(&ub, &ua) != common(&ua, &ub) {
        return Err("the order of other languages changed".into());
    }
    // A language whose input methods changed may have been moved by Windows,
    // but only to the end of the list (what InstallLayoutOrTip does). Anywhere
    // else the move could be the user's, and KeyDock cannot tell who made it
    // (review TP-01): strip the trailing run of touched or new languages; the
    // rest must keep the order it had before.
    let lower = |l: &String| l.to_ascii_lowercase();
    let mut keep = after.languages.len();
    while keep > 0 {
        let l = &after.languages[keep - 1];
        if touched.contains(&lower(l)) || !before.has_language(l) {
            keep -= 1;
        } else {
            break;
        }
    }
    let prefix: Vec<String> = after.languages[..keep]
        .iter()
        .filter(|l| before.has_language(l))
        .map(lower)
        .collect();
    let before_order: Vec<String> = before
        .languages
        .iter()
        .map(lower)
        .filter(|l| prefix.contains(l))
        .collect();
    if prefix != before_order {
        return Err(
            "a language whose layouts changed was moved somewhere other than the end of the list; that may have been you"
                .into(),
        );
    }
    change.first_removed = before.languages.first().filter(|f| !after.has_language(f)).cloned();
    Ok(change)
}

/// After a restore: `now` must have the order `want` and otherwise equal
/// `expected` (the snapshot the restore started from); only the order
/// numbers of input methods may differ. `Err` describes the difference.
pub fn verify_restored(expected: &Snapshot, now: &Snapshot, want: &[String]) -> Result<(), String> {
    let same_order =
        now.languages.len() == want.len() && now.languages.iter().zip(want).all(|(a, b)| a.eq_ignore_ascii_case(b));
    if !same_order {
        return Err(format!(
            "the order is {} (wanted {})",
            now.languages.join(", "),
            want.join(", ")
        ));
    }
    if now.root != expected.root {
        return Err("other language settings changed".into());
    }
    let keys = |s: &Snapshot| s.subkeys.keys().cloned().collect::<BTreeSet<_>>();
    if keys(now) != keys(expected) {
        return Err("the set of languages changed".into());
    }
    for (tag, values) in &expected.subkeys {
        let n = &now.subkeys[tag];
        if expected.tips(tag) != now.tips(tag) || !values_equal_except_tip_data(values, n) {
            return Err(format!("the settings of {tag} changed"));
        }
    }
    Ok(())
}

/// A Windows PowerShell script that reorders the current user language list
/// to `order`, but only if the list still matches `expect` (languages in
/// order, each with its input methods) when the script reads it. Exit code
/// [`CONFLICT_EXIT`] when it does not; nothing is written then. `None` if a
/// tag or input method is not plain.
///
/// The list is flattened into an array first (`Get-WinUserLanguageList`
/// writes one list object; with one language a substitute that writes items
/// would otherwise yield a single object), so indexing and `.Count` work the
/// same for one language as for several.
///
/// The script uses only pipelines, operators and cmdlets, no .NET method
/// calls, so it also runs under Constrained Language Mode (WDAC, AppLocker);
/// the cmdlet's parameter binding turns the array into its list type.
pub fn restore_script(order: &[String], expect: &[(String, Vec<String>)]) -> Option<String> {
    if order.is_empty()
        || !order.iter().all(|t| is_safe_tag(t))
        || !expect
            .iter()
            .all(|(t, tips)| is_safe_tag(t) && tips.iter().all(|x| is_safe_tip(x)))
    {
        return None;
    }
    let tags = order.iter().map(|t| format!("'{t}'")).collect::<Vec<_>>().join(",");
    let exp = expect
        .iter()
        .map(|(t, tips)| format!("'{t}|{}'", tips.join(",")))
        .collect::<Vec<_>>()
        .join(",");
    Some(format!(
        "$ErrorActionPreference = 'Stop'; \
         $want = @({tags}); \
         $expect = @({exp}); \
         $list = @(Get-WinUserLanguageList | ForEach-Object {{ $_ }}); \
         $same = $list.Count -eq $expect.Count; \
         for ($i = 0; $same -and $i -lt $expect.Count; $i++) {{ \
             $e = $expect[$i] -split '\\|'; \
             $tips = @($list[$i].InputMethodTips); \
             $et = @($e[1] -split ',' | Where-Object {{ $_ }}); \
             if ($list[$i].LanguageTag -ne $e[0] -or $tips.Count -ne $et.Count) {{ $same = $false }} \
             else {{ foreach ($t in $et) {{ if ($tips -notcontains $t) {{ $same = $false }} }} }} \
         }}; \
         if (-not $same) {{ exit {CONFLICT_EXIT} }}; \
         $new = @(foreach ($t in $want) {{ $list | Where-Object {{ $_.LanguageTag -eq $t }} }}) + \
                @($list | Where-Object {{ $want -notcontains $_.LanguageTag }}); \
         if ($new.Count -ne $list.Count) {{ exit {CONFLICT_EXIT} }}; \
         Set-WinUserLanguageList -LanguageList $new -Force -WarningAction SilentlyContinue"
    ))
}

/// Exit code of [`restore_script`] when the list changed before it could write.
pub const CONFLICT_EXIT: u32 = 3;

/// How KeyDock deals with a language order changed by its layout change
/// (`language_order` in config.toml).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OrderPolicy {
    /// Opt-in: put the order back with `Set-WinUserLanguageList` when every
    /// difference is explained by KeyDock's own change (see the module
    /// documentation for what remains possible).
    Restore,
    /// The default since 0.3.5: never write the language list; only report
    /// a changed order. Layout changes can still change the list.
    #[default]
    Report,
}

/// The restore scripts of KeyDock 0.3.2 and 0.3.3, which ran PowerShell
/// outside any job, so a restore of theirs can outlive them (review S7-02).
/// Copied verbatim from those versions' `reorder_script`, with `tags` the
/// quoted language list.
fn historical_scripts(tags: &str) -> [String; 2] {
    [
        // 0.3.2
        format!(
            "$ErrorActionPreference = 'Stop'; \
             $want = @({tags}); \
             $list = Get-WinUserLanguageList; \
             $new = [Activator]::CreateInstance($list.GetType()); \
             foreach ($t in $want) {{ foreach ($l in $list) {{ if ($l.LanguageTag -eq $t) {{ $new.Add($l) }} }} }}; \
             foreach ($l in $list) {{ if (-not ($want -contains $l.LanguageTag)) {{ $new.Add($l) }} }}; \
             if ($new.Count -ne $list.Count) {{ throw 'language list changed while reordering' }}; \
             Set-WinUserLanguageList -LanguageList $new -Force -WarningAction SilentlyContinue; \
             (Get-WinUserLanguageList | ForEach-Object {{ $_.LanguageTag }}) -join ', '"
        ),
        // 0.3.3
        format!(
            "$ErrorActionPreference = 'Stop'; \
             $want = @({tags}); \
             $list = Get-WinUserLanguageList; \
             $new = @(foreach ($t in $want) {{ $list | Where-Object {{ $_.LanguageTag -eq $t }} }}) + \
                    @($list | Where-Object {{ $want -notcontains $_.LanguageTag }}); \
             if ($new.Count -ne $list.Count) {{ throw 'language list changed while reordering' }}; \
             Set-WinUserLanguageList -LanguageList $new -Force -WarningAction SilentlyContinue"
        ),
    ]
}

/// The switches before the script, per version (same order as above).
const HISTORICAL_SWITCHES: [&[&str]; 2] = [
    &[
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
    ],
    &["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"],
];

/// Splits a Windows command line into arguments the way the C runtime (and
/// `CommandLineToArgvW`) does for every argument after the program name:
/// whitespace separates, double quotes group, `2n` backslashes before a
/// quote become `n` and start or end a group, `2n+1` backslashes become `n`
/// and a literal quote, other backslashes are literal. The program name is
/// taken up to the next whitespace, or between quotes, without escapes.
pub fn split_command_line(line: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut it = line.chars().peekable();
    // The program name.
    let mut prog = String::new();
    if it.peek() == Some(&'"') {
        it.next();
        for c in it.by_ref() {
            if c == '"' {
                break;
            }
            prog.push(c);
        }
    } else {
        while let Some(&c) = it.peek() {
            if c == ' ' || c == '\t' {
                break;
            }
            prog.push(c);
            it.next();
        }
    }
    args.push(prog);
    loop {
        while matches!(it.peek(), Some(' ') | Some('\t')) {
            it.next();
        }
        if it.peek().is_none() {
            break;
        }
        let mut arg = String::new();
        let mut quoted = false;
        while let Some(&c) = it.peek() {
            match c {
                ' ' | '\t' if !quoted => break,
                '\\' => {
                    let mut n = 0;
                    while it.peek() == Some(&'\\') {
                        it.next();
                        n += 1;
                    }
                    if it.peek() == Some(&'"') {
                        arg.extend(std::iter::repeat_n('\\', n / 2));
                        if n % 2 == 1 {
                            arg.push('"');
                            it.next();
                        }
                    } else {
                        arg.extend(std::iter::repeat_n('\\', n));
                    }
                }
                '"' => {
                    it.next();
                    if quoted && it.peek() == Some(&'"') {
                        // "" inside quotes: a literal quote.
                        arg.push('"');
                        it.next();
                    } else {
                        quoted = !quoted;
                    }
                }
                _ => {
                    arg.push(c);
                    it.next();
                }
            }
        }
        args.push(arg);
    }
    args
}

/// Whether `command_line` is a language-order restore of KeyDock 0.3.2 or
/// 0.3.3 (review S7-02): Windows PowerShell started with exactly that
/// version's switches and, as the one script argument, exactly that
/// version's script around a list of plain language tags. A fragment of it
/// in a comment, in quoted data or in another script does not match. This
/// is a compatibility check for those two versions only; later versions run
/// their restore in a job that ends with them.
pub fn is_historical_restore(command_line: &str) -> bool {
    let args = split_command_line(command_line);
    let Some(prog) = args.first() else { return false };
    let name = prog.rsplit(['\\', '/']).next().unwrap_or(prog);
    if !name.eq_ignore_ascii_case("powershell.exe") && !name.eq_ignore_ascii_case("powershell") {
        return false;
    }
    const MARK: &str = "\u{1}";
    let templates = historical_scripts(MARK);
    HISTORICAL_SWITCHES
        .iter()
        .zip(templates.iter())
        .any(|(switches, template)| {
            let rest = &args[1..];
            if rest.len() != switches.len() + 1 || !rest.iter().zip(switches.iter()).all(|(a, s)| a == s) {
                return false;
            }
            let script = &rest[switches.len()];
            let (pre, post) = template.split_once(MARK).expect("the mark is in the template");
            script.len() > pre.len() + post.len()
                && script.starts_with(pre)
                && script.ends_with(post)
                && is_tag_list(&script[pre.len()..script.len() - post.len()])
        })
}

/// `'tag','tag',...` with plain language tags (as those versions wrote it).
fn is_tag_list(s: &str) -> bool {
    !s.is_empty()
        && s.split(',')
            .all(|t| t.len() >= 3 && t.starts_with('\'') && t.ends_with('\'') && is_safe_tag(&t[1..t.len() - 1]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Command lines exactly as KeyDock 0.3.2 and 0.3.3 started them: the
    /// program quoted, and the script quoted the way Rust's standard library
    /// quotes an argument with spaces (it has no double quotes).
    fn historical_command_lines(tags: &str) -> [String; 2] {
        let s = historical_scripts(tags);
        let exe = "\"C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe\"";
        [
            format!(
                "{exe} -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command \"{}\"",
                s[0]
            ),
            format!("{exe} -NoLogo -NoProfile -NonInteractive -Command \"{}\"", s[1]),
        ]
    }

    #[test]
    fn command_lines_split_like_windows() {
        assert_eq!(split_command_line("a.exe b c"), v(&["a.exe", "b", "c"]));
        assert_eq!(
            split_command_line("\"C:\\Program Files\\x.exe\" \"a b\" c\\d"),
            v(&["C:\\Program Files\\x.exe", "a b", "c\\d"])
        );
        assert_eq!(
            split_command_line(r#"x "a\"b" c\\"d e" f"#),
            v(&["x", "a\"b", "c\\d e", "f"])
        );
        assert_eq!(split_command_line(r#"x "a""b" """#), v(&["x", "a\"b", ""]));
    }

    #[test]
    fn historical_restores_are_recognized() {
        // Review S7-02: the real shapes of both versions, with one or more
        // languages, and a bare program name.
        for tags in ["'en-US'", "'en-US','ru'", "'de-DE','en-US','zh-Hant-TW'"] {
            for c in historical_command_lines(tags) {
                assert!(is_historical_restore(&c), "{c}");
            }
        }
        let s = &historical_scripts("'ru','en-US'")[1];
        assert!(is_historical_restore(&format!(
            "powershell.exe -NoLogo -NoProfile -NonInteractive -Command \"{s}\""
        )));
        // The current script is not one of them (it runs in a job).
        let current = restore_script(
            &v(&["ru", "en-US"]),
            &[
                ("ru".into(), v(&["0419:00000419"])),
                ("en-US".into(), v(&["0409:00000409"])),
            ],
        )
        .unwrap();
        assert!(!is_historical_restore(&format!(
            "powershell.exe -NoLogo -NoProfile -NonInteractive -Command \"{current}\""
        )));
    }

    #[test]
    fn fragments_comments_data_and_other_scripts_are_not_restores() {
        // Review S7-02.
        let setter = "Set-WinUserLanguageList -LanguageList $new -Force -WarningAction SilentlyContinue";
        let real = &historical_command_lines("'en-US','ru'")[1];
        let s = historical_scripts("'en-US','ru'");
        for c in [
            // The setter in a comment, in data, in a script of its own.
            format!("powershell.exe -NoProfile -NonInteractive -Command Start-Sleep -Seconds 300; # {setter}"),
            format!("powershell.exe -NoLogo -NoProfile -NonInteractive -Command \"Write-Output '{setter}'\""),
            format!("powershell.exe -NoLogo -NoProfile -NonInteractive -Command \"$new = Get-WinUserLanguageList; {setter}\""),
            // The historical script with something appended or prepended.
            format!("powershell.exe -NoLogo -NoProfile -NonInteractive -Command \"{}; Start-Sleep 600\"", s[1]),
            format!("powershell.exe -NoLogo -NoProfile -NonInteractive -Command \"Start-Sleep 600; {}\"", s[1]),
            format!("powershell.exe -NoLogo -NoProfile -NonInteractive -Command \"{} # x\"", s[1]),
            // The script as a comment, or as quoted data.
            format!("powershell.exe -NoLogo -NoProfile -NonInteractive -Command \"# {}\"", s[1]),
            format!("powershell.exe -NoLogo -NoProfile -NonInteractive -Command \"'{}'\"", s[1].replace('\'', "''")),
            // Other switches, or the 0.3.2 switches with the 0.3.3 script.
            format!("powershell.exe -NoProfile -NonInteractive -Command \"{}\"", s[1]),
            format!("powershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command \"{}\"", s[1]),
            format!("powershell.exe -NoLogo -NoProfile -NonInteractive -Command \"{}\" extra", s[1]),
            // Another program with the real arguments.
            real.replace("powershell.exe", "pwsh.exe"),
            // Tags that are not plain language tags (an injected statement).
            historical_command_lines("'en-US'); Start-Sleep 600; @('ru'")[1].clone(),
            historical_command_lines("")[1].clone(),
        ] {
            assert!(!is_historical_restore(&c), "{c}");
        }
    }

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    fn dword(n: u32) -> RegValue {
        RegValue {
            ty: 4,
            data: n.to_le_bytes().to_vec(),
        }
    }

    fn text(s: &str) -> RegValue {
        RegValue {
            ty: 1,
            data: s.as_bytes().to_vec(),
        }
    }

    /// A snapshot: languages in order, each with its input methods.
    fn snap(langs: &[(&str, &[&str])]) -> Snapshot {
        let mut s = Snapshot {
            languages: langs.iter().map(|(t, _)| t.to_string()).collect(),
            ..Default::default()
        };
        s.root.insert("showtextprediction".into(), dword(1));
        for (tag, tips) in langs {
            let mut m = BTreeMap::new();
            m.insert("cachedlanguagename".into(), text(tag));
            for (i, t) in tips.iter().enumerate() {
                m.insert(t.to_ascii_lowercase(), dword(i as u32 + 1));
            }
            s.subkeys.insert(tag.to_ascii_lowercase(), m);
        }
        s
    }

    #[test]
    fn parses_multi_sz() {
        let data: Vec<u16> = "en-US\0ru\0\0".encode_utf16().collect();
        assert_eq!(parse_multi_sz(&data), v(&["en-US", "ru"]));
        assert!(parse_multi_sz(&[0, 0]).is_empty());
        assert!(parse_multi_sz(&[]).is_empty());
    }

    #[test]
    fn same_order_needs_nothing() {
        assert_eq!(restore_order(&v(&["en-US", "ru"]), &v(&["en-US", "ru"])), None);
        assert_eq!(restore_order(&v(&["en-US", "ru"]), &v(&["en-us", "ru"])), None);
        // A language added at the end does not change the order of the others.
        assert_eq!(restore_order(&v(&["en-US", "ru"]), &v(&["en-US", "ru", "de-DE"])), None);
        // A removed language is not brought back.
        assert_eq!(restore_order(&v(&["en-US", "ru", "de-DE"]), &v(&["en-US", "ru"])), None);
    }

    #[test]
    fn moved_language_is_put_back() {
        // The reported case: en-US was first, ended up after ru.
        assert_eq!(
            restore_order(&v(&["en-US", "ru"]), &v(&["ru", "en-US"])),
            Some(v(&["en-US", "ru"]))
        );
        // A new language inserted in front goes after the existing ones.
        assert_eq!(
            restore_order(&v(&["en-US", "ru"]), &v(&["de-DE", "en-US", "ru"])),
            Some(v(&["en-US", "ru", "de-DE"]))
        );
        assert_eq!(
            restore_order(&v(&["en-US", "ru", "de-DE"]), &v(&["ru", "de-DE", "en-US"])),
            Some(v(&["en-US", "ru", "de-DE"]))
        );
    }

    #[test]
    fn users_move_of_a_touched_language_is_not_restored() {
        // Review TP-01: en-US gets another layout while the user puts ru first.
        let before = snap(&[
            ("en-US", &["0409:00000409"]),
            ("ru", &["0419:00000419"]),
            ("de-DE", &["0407:00000407"]),
        ]);
        let after = snap(&[
            ("ru", &["0419:00000419"]),
            ("en-US", &["0409:00000409", "0409:00010409"]),
            ("de-DE", &["0407:00000407"]),
        ]);
        let inst = v(&["0409:00010409"]);
        let e = explain_change(&before, &after, &inst, &[]).unwrap_err();
        assert!(e.contains("may have been you"), "{e}");
        // The same language moved to the very end looks exactly like Windows'
        // side effect; it is accepted (and restored only with the opt-in).
        let at_end = snap(&[
            ("ru", &["0419:00000419"]),
            ("de-DE", &["0407:00000407"]),
            ("en-US", &["0409:00000409", "0409:00010409"]),
        ]);
        assert!(explain_change(&before, &at_end, &inst, &[]).is_ok());
        // No move at all is fine too.
        let same = snap(&[
            ("en-US", &["0409:00000409", "0409:00010409"]),
            ("ru", &["0419:00000419"]),
            ("de-DE", &["0407:00000407"]),
        ]);
        assert!(explain_change(&before, &same, &inst, &[]).is_ok());
        assert_eq!(restore_order(&before.languages, &same.languages), None);
    }

    #[test]
    fn only_attempted_changes_explain_the_list() {
        // Review FP-03: KeyDock enables a Russian variant; meanwhile another
        // program enables an English variant and the user moves English to
        // the end. The observed delta has both additions.
        let before = snap(&[
            ("en-US", &["0409:00000409"]),
            ("ru", &["0419:00000419"]),
            ("de-DE", &["0407:00000407"]),
        ]);
        let after = snap(&[
            ("ru", &["0419:00000419", "0419:00010419"]),
            ("de-DE", &["0407:00000407"]),
            ("en-US", &["0409:00000409", "0409:00010409"]),
        ]);
        let attempted = v(&["0419:00010419"]);
        let observed = v(&["0419:00010419", "0409:00010409"]);
        // What the engine passes now: the attempted batch only. The English
        // addition is unexplained, so nothing is restored.
        let e = explain_change(&before, &after, &attempted, &[]).unwrap_err();
        assert!(e.contains("appeared for en-us"), "{e}");
        // What 0.3.5 passed (attempted plus observed): the English change
        // would have counted as KeyDock's, and its tail move been undone.
        assert!(explain_change(&before, &after, &observed, &[]).is_ok());
    }

    #[test]
    fn report_is_the_default_policy() {
        assert_eq!(OrderPolicy::default(), OrderPolicy::Report);
    }

    #[test]
    fn own_layout_change_is_explained() {
        // en-US: US -> Spanish layout; Windows moved en-US to the end.
        let before = snap(&[("en-US", &["0409:00000409"]), ("ru", &["0419:00000419"])]);
        let after = snap(&[("ru", &["0419:00000419"]), ("en-US", &["0409:0000040A"])]);
        let c = explain_change(&before, &after, &v(&["0409:0000040A"]), &v(&["0409:00000409"])).unwrap();
        assert_eq!(c, Change::default());
        assert_eq!(
            restore_order(&before.languages, &after.languages),
            Some(v(&["en-US", "ru"]))
        );
    }

    #[test]
    fn removing_the_first_languages_last_layout_is_reported() {
        // Review KD-01: [en-US, ru] -> [ru] needs no reorder, but the first
        // language (and with it the display language) is gone.
        let before = snap(&[("en-US", &["0409:00000409"]), ("ru", &["0419:00000419"])]);
        let after = snap(&[("ru", &["0419:00000419"])]);
        let c = explain_change(&before, &after, &[], &v(&["0409:00000409"])).unwrap();
        assert_eq!(c.removed, v(&["en-US"]));
        assert_eq!(c.first_removed.as_deref(), Some("en-US"));
        assert_eq!(restore_order(&before.languages, &after.languages), None);
    }

    #[test]
    fn foreign_changes_are_conflicts() {
        let before = snap(&[
            ("en-US", &["0409:00000409"]),
            ("ru", &["0419:00000419"]),
            ("de-DE", &["0407:00000407"]),
        ]);
        let inst = v(&["0409:0000040A"]);
        let rem = v(&["0409:00000409"]);
        let ours = || {
            snap(&[
                ("ru", &["0419:00000419"]),
                ("de-DE", &["0407:00000407"]),
                ("en-US", &["0409:0000040A"]),
            ])
        };
        assert!(explain_change(&before, &ours(), &inst, &rem).is_ok());

        // The user reordered two other languages meanwhile.
        let mut a = ours();
        a.languages = v(&["de-DE", "ru", "en-US"]);
        assert!(explain_change(&before, &a, &inst, &rem).is_err());
        // The user added an input method to another language.
        let mut a = ours();
        a.subkeys
            .get_mut("ru")
            .unwrap()
            .insert("0419:00010419".into(), dword(2));
        assert!(explain_change(&before, &a, &inst, &rem).is_err());
        // A property of another language changed (spelling, handwriting, ...).
        let mut a = ours();
        a.subkeys
            .get_mut("de-de")
            .unwrap()
            .insert("spellchecking".into(), dword(0));
        assert!(explain_change(&before, &a, &inst, &rem).is_err());
        // A global setting changed.
        let mut a = ours();
        a.root.insert("showtextprediction".into(), dword(0));
        assert!(explain_change(&before, &a, &inst, &rem).is_err());
        // Another language was added or removed.
        let mut a = ours();
        a.languages.push("fr-FR".into());
        a.subkeys.insert(
            "fr-fr".into(),
            BTreeMap::from([("040c:0000040c".to_string(), dword(1))]),
        );
        assert!(explain_change(&before, &a, &inst, &rem).is_err());
        let a = snap(&[("ru", &["0419:00000419"]), ("en-US", &["0409:0000040A"])]);
        assert!(explain_change(&before, &a, &inst, &rem).is_err());
    }

    #[test]
    fn new_language_from_our_install_is_explained() {
        let before = snap(&[("en-US", &["0409:00000409"])]);
        let after = snap(&[("de-DE", &["0407:00000407"]), ("en-US", &["0409:00000409"])]);
        let c = explain_change(&before, &after, &v(&["0407:00000407"]), &[]).unwrap();
        assert_eq!(c.added, v(&["de-DE"]));
        assert_eq!(
            restore_order(&before.languages, &after.languages),
            Some(v(&["en-US", "de-DE"]))
        );
    }

    #[test]
    fn verification_after_restore() {
        let expected = snap(&[("ru", &["0419:00000419"]), ("en-US", &["0409:0000040A"])]);
        let want = v(&["en-US", "ru"]);
        let mut now = expected.clone();
        now.languages = want.clone();
        // Renumbered input methods are fine.
        now.subkeys
            .get_mut("en-us")
            .unwrap()
            .insert("0409:0000040a".into(), dword(7));
        assert!(verify_restored(&expected, &now, &want).is_ok());
        // Wrong order.
        assert!(verify_restored(&expected, &expected, &want).is_err());
        // The restore dropped an input method.
        let mut bad = now.clone();
        bad.subkeys.get_mut("ru").unwrap().remove("0419:00000419");
        assert!(verify_restored(&expected, &bad, &want).is_err());
        // The restore changed another property.
        let mut bad = now.clone();
        bad.subkeys
            .get_mut("ru")
            .unwrap()
            .insert("handwriting".into(), dword(1));
        assert!(verify_restored(&expected, &bad, &want).is_err());
    }

    #[test]
    fn script_checks_the_list_before_writing() {
        let expect = vec![
            ("ru".to_string(), v(&["0419:00000419"])),
            ("en-US".to_string(), v(&["0409:0000040A"])),
        ];
        let s = restore_script(&v(&["en-US", "ru"]), &expect).unwrap();
        assert!(s.contains("$want = @('en-US','ru')"));
        assert!(s.contains("$expect = @('ru|0419:00000419','en-US|0409:0000040A')"));
        assert!(s.contains("exit 3"));
        // The check comes before the only write.
        assert!(s.find("exit 3").unwrap() < s.find("Set-WinUserLanguageList").unwrap());
        assert!(
            !s.contains("[Activator]") && !s.contains(".Add("),
            "no .NET method calls (Constrained Language Mode)"
        );
        assert!(restore_script(&v(&["en-US", "x'; rm"]), &expect).is_none());
        let bad = vec![("ru".to_string(), v(&["0419:0000'; rm"]))];
        assert!(restore_script(&v(&["ru"]), &bad).is_none());
        assert!(restore_script(&[], &expect).is_none());
        assert!(is_safe_tag("zh-Hant-TW"));
        assert!(!is_safe_tag(""));
        assert!(is_safe_tip(
            "0411:{03B5835F-F03C-411B-9CE2-AA23E1171E36}{A76C93D9-5523-4E90-AAFA-4DB112F9AC76}"
        ));
        assert!(is_tip_name("0409:00000409") && !is_tip_name("CachedLanguageName"));
    }
}
