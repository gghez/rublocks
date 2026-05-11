//! Project locale: BCP 47 tag parsing and built-in localized string tables.
//!
//! The project's `language` field in `main.json` is the single source of
//! truth for the default locale. Codegen threads it into `<html lang="...">`,
//! the `Content-Language` response header, and the dev-mode error overlay.
//!
//! Validation here is intentionally narrow: we accept the well-formed BCP 47
//! shape (`language[-script][-region][-variant...]`) but do not consult the
//! IANA registry. That keeps the binary dependency-free and matches the
//! issue #14 contract — the field exists so the contract is explicit, the
//! parser only blocks obviously-malformed values.

use regex::Regex;
use std::sync::OnceLock;

/// Validate a BCP 47 language tag against the well-formed shape.
///
/// Accepts the canonical `language[-script][-region][-variant...]` layout
/// without privateuse/extensions (the latter are valid BCP 47 but rublocks
/// does not need them and rejecting unknown tail subtags catches typos like
/// `fr_FR` or `francais` that an agent might emit). Case is irrelevant —
/// `fr-FR` and `FR-fr` both validate.
pub fn is_well_formed(tag: &str) -> bool {
    if tag.is_empty() {
        return false;
    }
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        // language: 2-3 ASCII letters (ISO 639-1/-2)
        // script:   4 ASCII letters (ISO 15924), optional
        // region:   2 letters (ISO 3166-1) or 3 digits (UN M.49), optional
        // variant:  5-8 alphanumerics or 4 starting with a digit, repeatable
        Regex::new(
            r"^(?i)[a-z]{2,3}(-[a-z]{4})?(-([a-z]{2}|[0-9]{3}))?(-([a-z0-9]{5,8}|[0-9][a-z0-9]{3}))*$",
        )
        .expect("BCP 47 regex compiles")
    });
    re.is_match(tag)
}

/// Localized strings used by the dev-mode error overlay.
///
/// Each variant maps onto one piece of UI copy. The set is intentionally
/// small — the overlay's bulk is file paths and trace dumps, both of which
/// stay locale-neutral.
#[derive(Debug, Clone, Copy)]
pub enum DevString {
    ManifestTitle,
    CodegenTitle,
    BuildTitle,
    ServicesTitle,
    RuntimeTitle,
    ManifestHint,
    CodegenHint,
    BuildHint,
    ServicesHint,
    RuntimeHint,
    LabelFile,
    LabelAt,
    LabelCode,
    LabelMessage,
    CopyButton,
    CopiedFeedback,
    CopyFailed,
    CargoOutput,
    Footer,
}

/// Pick the localized string for a key in the requested tag, falling back
/// to English for any tag we don't ship a table for.
///
/// We compare on the language subtag only (the part before the first `-`),
/// so `fr`, `fr-FR`, `fr-CA` all resolve to the same French strings. This is
/// a deliberate simplification — issue #14 ships exactly two locales and
/// per-region copy can wait for an actual user requesting it.
pub fn dev_string(tag: &str, key: DevString) -> &'static str {
    let lang = primary_subtag(tag);
    match lang.to_ascii_lowercase().as_str() {
        "fr" => fr(key),
        _ => en(key),
    }
}

/// True when the tag's primary subtag is one we ship localized strings for.
/// Used by codegen to emit a build-time warning ("falling back to en-US")
/// for any other tag.
pub fn has_dev_strings(tag: &str) -> bool {
    matches!(
        primary_subtag(tag).to_ascii_lowercase().as_str(),
        "en" | "fr"
    )
}

fn primary_subtag(tag: &str) -> &str {
    tag.split_once('-').map(|(p, _)| p).unwrap_or(tag)
}

fn en(key: DevString) -> &'static str {
    match key {
        DevString::ManifestTitle => "Manifest error",
        DevString::CodegenTitle => "Codegen error",
        DevString::BuildTitle => "Build error",
        DevString::ServicesTitle => "Services error",
        DevString::RuntimeTitle => "Runtime error",
        DevString::ManifestHint => {
            "A declarative file failed to parse. Fix the syntax / shape and save \u{2014} the page will reload automatically."
        }
        DevString::CodegenHint => {
            "rublocks could not produce a valid Rust project from the manifest. The message below comes straight from the compiler."
        }
        DevString::BuildHint => {
            "The generated Rust project failed to compile. The first cargo diagnostic is summarized below; the full output follows."
        }
        DevString::ServicesHint => {
            "A declared service could not be provisioned. Check that Docker is running, or export the env var the manifest references."
        }
        DevString::RuntimeHint => {
            "The dist binary exited before serving requests. The captured output is below."
        }
        DevString::LabelFile => "file",
        DevString::LabelAt => "at",
        DevString::LabelCode => "code",
        DevString::LabelMessage => "message",
        DevString::CopyButton => "Copy for agent",
        DevString::CopiedFeedback => "Copied",
        DevString::CopyFailed => "Copy failed \u{2014} select the trace manually",
        DevString::CargoOutput => "cargo output",
        DevString::Footer => {
            "rublocks dev mode \u{B7} this page auto-reloads when the issue is fixed"
        }
    }
}

fn fr(key: DevString) -> &'static str {
    match key {
        DevString::ManifestTitle => "Erreur de manifeste",
        DevString::CodegenTitle => "Erreur de g\u{E9}n\u{E9}ration",
        DevString::BuildTitle => "Erreur de compilation",
        DevString::ServicesTitle => "Erreur de service",
        DevString::RuntimeTitle => "Erreur d\u{2019}ex\u{E9}cution",
        DevString::ManifestHint => {
            "Un fichier d\u{E9}claratif n\u{2019}a pas pu \u{EA}tre analys\u{E9}. Corrigez la syntaxe ou la forme et sauvegardez \u{2014} la page se rechargera automatiquement."
        }
        DevString::CodegenHint => {
            "rublocks n\u{2019}a pas pu produire un projet Rust valide \u{E0} partir du manifeste. Le message ci-dessous provient directement du compilateur."
        }
        DevString::BuildHint => {
            "La compilation du projet Rust g\u{E9}n\u{E9}r\u{E9} a \u{E9}chou\u{E9}. La premi\u{E8}re erreur de cargo est r\u{E9}sum\u{E9}e ci-dessous ; la sortie compl\u{E8}te suit."
        }
        DevString::ServicesHint => {
            "Un service d\u{E9}clar\u{E9} n\u{2019}a pas pu \u{EA}tre provisionn\u{E9}. V\u{E9}rifiez que Docker est lanc\u{E9}, ou exportez la variable d\u{2019}environnement r\u{E9}f\u{E9}renc\u{E9}e par le manifeste."
        }
        DevString::RuntimeHint => {
            "Le binaire dist s\u{2019}est arr\u{EA}t\u{E9} avant de servir des requ\u{EA}tes. La sortie captur\u{E9}e est ci-dessous."
        }
        DevString::LabelFile => "fichier",
        DevString::LabelAt => "\u{E0}",
        DevString::LabelCode => "code",
        DevString::LabelMessage => "message",
        DevString::CopyButton => "Copier pour l\u{2019}agent",
        DevString::CopiedFeedback => "Copi\u{E9}",
        DevString::CopyFailed => {
            "\u{C9}chec de la copie \u{2014} s\u{E9}lectionnez la trace manuellement"
        }
        DevString::CargoOutput => "sortie cargo",
        DevString::Footer => {
            "rublocks dev mode \u{B7} cette page se recharge d\u{E8}s que le probl\u{E8}me est corrig\u{E9}"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_formed_accepts_canonical_tags() {
        for tag in [
            "en",
            "fr",
            "en-US",
            "fr-FR",
            "pt-BR",
            "zh-Hant",
            "zh-Hant-HK",
            "de-CH-1996",
            "es-419",
        ] {
            assert!(is_well_formed(tag), "{tag} should be well-formed");
        }
    }

    #[test]
    fn well_formed_is_case_insensitive() {
        assert!(is_well_formed("EN-us"));
        assert!(is_well_formed("zh-hant-hk"));
    }

    #[test]
    fn well_formed_rejects_obvious_typos() {
        for tag in [
            "", "francais", "fr_FR", "fr--FR", "f", "fr-FRA", "fr-FR-", "1234", "fr FR",
        ] {
            assert!(!is_well_formed(tag), "{tag} should be rejected");
        }
    }

    #[test]
    fn dev_string_picks_french_for_fr_subtag() {
        assert_eq!(
            dev_string("fr-FR", DevString::ManifestTitle),
            "Erreur de manifeste"
        );
        assert_eq!(
            dev_string("fr-CA", DevString::ManifestTitle),
            "Erreur de manifeste"
        );
        assert_eq!(
            dev_string("fr", DevString::ManifestTitle),
            "Erreur de manifeste"
        );
    }

    #[test]
    fn dev_string_falls_back_to_english_for_unknown_tag() {
        assert_eq!(
            dev_string("pt-BR", DevString::ManifestTitle),
            "Manifest error"
        );
        assert_eq!(dev_string("zh", DevString::CodegenTitle), "Codegen error");
    }

    #[test]
    fn has_dev_strings_recognizes_shipped_locales() {
        assert!(has_dev_strings("en"));
        assert!(has_dev_strings("en-US"));
        assert!(has_dev_strings("fr"));
        assert!(has_dev_strings("fr-FR"));
        assert!(!has_dev_strings("pt-BR"));
        assert!(!has_dev_strings("de"));
    }
}
