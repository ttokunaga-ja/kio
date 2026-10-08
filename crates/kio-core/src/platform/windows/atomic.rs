pub(crate) fn atomic_relative_spelling_is_normalized(raw: &str) -> bool {
    !raw.split(['/', '\\'])
        .any(|part| part.is_empty() || part == "." || part == "..")
}
