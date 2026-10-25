# File-based interface localization

The desktop interface, tray menus, notifications, account setup, offline browser, activity history, and updater status use `src/i18n.rs`. Core protocol identifiers, database fields, filenames, CLI syntax, and raw diagnostic details are not translated.

## Language selection

The default is automatic system-language detection. POSIX locale priority is `LC_ALL` → `LC_MESSAGES` → `LANG`. Empty variables are ignored. GNU `LANGUAGE` can supply a colon-separated message-language preference list; it is ignored under C/POSIX, including `C.UTF-8`. Without a usable preference, English is selected.

Locale codes are normalized: `en_US.UTF-8` → `en`, `id_ID` → `id`, and `zh-CN`/`zh_Hans_CN` → `zh`. The bundled Chinese catalog uses Simplified Chinese. Unsupported languages fall back to English.

The dashboard includes an **Interface language** card. **System default** is always available; other choices are discovered from valid JSON catalogs in the same directories used for translations. Bundled choices are English, Indonesia, and 简体中文. **Reload languages** rescans the files while the dashboard stays open. Removed custom files disappear from the refreshed list. Duplicate codes are merged in override-directory order. **Apply language** saves the choice in `$XDG_CONFIG_HOME/mygdrive/desktop-settings.json`, then automatically closes/reopens the desktop window without stopping the sync service. The default configuration directory is `~/.config`. Invalid settings do not block startup; malformed or symlinked settings files are not overwritten.

Selection priority is `--language` → `MYGDRIVE_LANGUAGE` → saved dashboard choice → `auto`. An explicit `--language auto` overrides an environment preference. Relaunching preserves the current profile and the selected mode, including automatic system detection. Opening a profile already running through an external CLI command still reuses that existing process.

## Catalogs and search order

The JSON files are UTF-8 objects mapping message IDs to translated strings. Existing Indonesian source messages serve as stable message IDs; they are keys, not the default display language. New messages may use English keys. All bundled catalogs must have identical keys.

Defaults are embedded from `assets/locales/{en,id,zh}.json`, so the executable works even without external files. Startup overlays valid entries in this order:

1. Source checkout `assets/locales/`, when present.
2. Installed catalogs beside the executable: Debian `../assets/locales/` or AppImage `../share/mygdrive/locales/`.
3. `$XDG_CONFIG_HOME/mygdrive/locales/` (normally `~/.config/mygdrive/locales/`).
4. `MYGDRIVE_LOCALES_DIR`, when configured.

Later locations override earlier entries. No current-working-directory catalog is loaded. Language codes are restricted to two or three ASCII letters, preventing path traversal. For an unsupported language, a valid external catalog enables it; missing entries use English.

```bash
mkdir -p "$HOME/.config/mygdrive/locales"
printf '{"Jeda":"Pause personnalisée"}\n' > "$HOME/.config/mygdrive/locales/fr.json"
./run-desktop.sh --language fr

MYGDRIVE_LOCALES_DIR=/path/to/translations ./run-desktop.sh --language id
```

Restart the application after editing a file. Catalogs are loaded once per process, shared safely across UI and worker threads. Editing translations does not require a Rust rebuild. This implementation does not watch files for changes or replace an existing window's language live.

## Placeholders and fallback

Preserve each message's placeholders. Automatic `{}` arguments correspond to `{0}`, `{1}`, etc. Translators can use explicit numbered placeholders to reorder arguments. Named placeholders such as `{stage}`, `{status}`, and `{pending_files}` must retain their names. Values are interpolated once, so braces in a filename are not interpreted as translation tokens.

Malformed JSON files are ignored with a diagnostic. Empty translations or mismatched placeholders are ignored per entry. Unknown keys are ignored. Missing keys fall back to the bundled English message, then the source key if the executable does not know it. Invalid overrides do not prevent startup.

## Adding messages or languages

Add the same message key to all bundled catalogs and call `tr("key")` for plain text, or `fmt("key", &[("0", value.to_string())])` for runtime values. Do not translate action IDs, MIME types, API payloads, database states, paths, or CSS classes.

To add a language without changing/rebuilding the application, copy `en.json` to a lowercase two- or three-letter file such as `fr.json`, translate its values, and set `"_language_name": "Français"` for its dashboard label. Place it in an override directory, choose **Reload languages**, select **Français**, then **Apply language**. Without `_language_name`, a custom language displays its filename code. Malformed/non-string JSON catalogs and noncanonical filenames are skipped; missing translations still use English. The saved preference supports custom codes as well as bundled languages. CLI/environment selection remains available. To bundle a language by default, also register its embedded catalog in `src/i18n.rs`.

The Debian and AppImage build scripts package `assets/locales/*.json`. Tests verify locale precedence, catalog key/placeholder consistency, custom overrides, malformed files, English fallback, and safe argument reordering.

### Minimal additional language file

Save this UTF-8 file as `~/.config/mygdrive/locales/fr.json`, then reload the dashboard language list:

```json
{
  "_language_name": "Français",
  "Jeda": "Pause",
  "Lanjutkan": "Reprendre",
  "Interface language": "Langue de l’interface",
  "System default": "Langue du système",
  "Apply language": "Appliquer la langue",
  "Reload languages": "Recharger les langues"
}
```

A partial catalog is valid; untranslated messages fall back to English. `_language_name` is metadata, not an interface message ID. Selecting the new file reopens the UI to load its translations while the service continues running. Changes to existing message values load on the next reopen; **Reload languages** refreshes the list and display names.
