# Porting notes: Python helium-browser-agent → Rust `hba`

The Python package (`helium_browser_agent`, four commands built on browser-harness 0.1.13 and
agent-browser 0.37.0) is replaced by one Rust binary, `hba`, with its own DevTools client, daemon
and vendored Playwright page engine. This is a redesign, not a line-by-line port: the old
commands wrapped other tools and guarded what they could do; `hba` does the work itself, so whole
classes of old code and tests (relay filtering, backend environment scrubbing, startup-page reuse,
legacy-state migration) have nothing left to apply to.

## Command mapping

| Old | `hba` |
| --- | --- |
| `helium-browser-use --session S --profile P` + Python code | `hba -s S -p P COMMAND`; several steps: `hba run` |
| `--personal`, `--work` | `-p ALIAS` with `[aliases]` in the config |
| `--close` | `hba -s S close` |
| `--lock-timeout` | commands of one session queue in the daemon (up to 120 s); `--timeout` per command |
| `goto_url(url)`; `wait_for_load()` | `hba open URL`; `hba wait --load load` |
| `js(expr)` | `hba js EXPR` |
| `fill_input(sel, text)`; `input_value(sel)` | `hba fill TARGET TEXT`; `hba get value TARGET` |
| `wait_for_element(sel)` | `hba wait TARGET` |
| `list_tabs()`; `list_tabs(all_tabs=True)` | `hba tabs`; `hba tabs --all` |
| `new_tab(url)`; `switch_tab(t)`; `activate_tab(t)`; `close_tab(t)` | `hba tab new URL`; `hba tab use t`; `hba tab front t`; `hba tab close t` |
| `profiles()`; `profile_of(t)` | `hba profiles`; `hba tabs --all` (grouped by profile) |
| `page_info()` | `hba status`, `hba get url`, `hba get title` |
| `iframe_target(url)` | frame refs (`f1e3`) from `hba snap`; `hba js --frame TARGET` |
| `cdp(method, **params)` | `hba cdp METHOD JSON` |
| `download(adapter, sel, dest)` | `hba download TARGET -o DEST` |
| `agent-browser-helium open/snapshot -i/click/fill/type/press/get` | same verbs: `hba open`, `hba snap -i`, `hba click e1` (refs without `@`), … |
| `agent-browser-helium close`; `detach` | `hba close`; `hba daemon stop` (tabs stay open) |
| `helium-tab new --profile P --url U --foreground` | `hba -p P tab new U --front` |
| `helium-tab contexts/identify/profile-of/exists` | `hba profiles`, `hba tabs --all [--json]` |
| `helium-tab ws` | `hba browser endpoint` |
| `helium-tab close-browser` | dropped; close Helium yourself or `hba browser restart` |
| `helium-agent --version/profiles/doctor` | `hba --version`, `hba profiles`, `hba doctor` |
| `~/.local/bin/helium-restart` | `hba browser restart` |
| `~/.local/bin/helium-jev` | rewritten on `hba tab new --json` and `hba browser endpoint` |

## Test mapping

Rust tests are in `src/**` (unit), `tests/cli.rs` (daemon and configuration, no browser) and
`tests/live.rs` (end to end against a throwaway headless Helium, `HBA_LIVE=1`).

| Old test | Covered by |
| --- | --- |
| `test_missing_default_file_uses_defaults`, `test_environment_data_root_override` | `cli::profiles_are_read_from_local_state_without_a_browser` |
| `test_missing_explicit_file_fails` | `cli::a_missing_explicit_config_file_is_an_error` |
| `test_unknown_key_fails`, `test_non_string_profile_fails` | `config::unknown_keys_are_rejected`, `cli::invalid_config_is_reported_by_the_command` |
| `test_workspaces_validate_values` | `config::empty_values_are_rejected` |
| `test_longest_workspace_wins`, `test_current_workspace_defaults_and_prefix_boundary` | `config::longest_workspace_wins_on_component_boundaries` |
| `test_environment_profile_overrides_workspace` | `profiles::choice_order_is_flag_session_rule_then_last_used` |
| `test_custom_aliases_and_paths` | `cli::the_config_file_sets_the_data_dir_and_aliases`, `profiles::aliases_map_to_display_names` |
| `test_endpoint_rejects_wrong_socket_owner` | `cli::an_endpoint_owned_by_another_process_is_refused` |
| `test_endpoint_rejects_non_loopback`, `test_relay_metadata_cannot_redirect_to_external_host` | `cli::an_endpoint_listening_beyond_loopback_is_refused` |
| `test_empty_close_never_contacts_browser_or_starts_backend`, `test_empty_cleanup_does_not_contact_or_launch_browser`, `test_cleanup_endpoint_failure_does_not_launch` | `cli::closing_without_a_browser_never_starts_one` |
| `test_unknown_sessions_and_unscoped_page_commands_are_refused` | `cli::page_commands_without_a_tab_never_start_helium` |
| `test_close_preserves_state_when_browser_unavailable`, `test_failed_close_retains_state` | `cli::closing_while_helium_is_away_keeps_the_session_for_later` |
| `test_backend_inventory_excludes_user_and_other_profiles`, `test_foreign_target_attach_close_activate_are_refused`, `test_switch_close_activate_refuse_unowned_pages`, `test_javascript_refuses_foreign_targets` | `live::sessions_do_not_share_tabs`, `live::adopted_tabs_are_released_not_closed` |
| `test_popup_ownership_excludes_another_tasks_popup`, `test_child_and_grandchild_owned_but_unrelated_popup_excluded` | `live::popups_join_the_session_and_close_with_it` (popups are adopted only when their opener is the session's page) |
| `test_fill_validates_readonly_before_clearing`, `test_fill_value_mismatch_is_reported_without_retry` | `live::pointer_and_form_controls` |
| `test_fill_rejects_multiline_single_line_before_inserting` | `live::fill_type_and_submit` |
| `test_dialog_opening_during_page_info_is_reported` | `live::dialogs_block_until_answered`, `live::keyboard_and_prompts` |
| `test_out_of_process_frames_require_owned_ancestry`, `test_iframe_lookup_is_scoped_to_owned_page`, `test_iframe_ancestry_remains_scoped` | `live::frames_are_stitched_and_actionable` (frames are reached only through the tab's own sessions) |
| `test_invalid_lock_timeouts_are_refused` | `cli::timeouts_must_be_positive_and_at_most_a_day` (unit), `cli::a_timeout_longer_than_a_day_is_refused` |
| `test_corrupt_ownership_state_fails_closed`, `test_corrupt_ownership_fails_closed`, `test_corrupt_cache_recovers`, `test_invalid_cache_value_recovers` | `Store::load` sets a damaged `state.json` aside and starts empty; no listed tab is closed (see differences) |
| `test_state_profile_binding_survives_generation_change`, `test_stale_context_is_discarded` | `session::a_new_browser_run_drops_ids_but_keeps_sessions` |
| `test_cli_preserves_profile_and_command_values` | `cli::cli_definition_is_consistent` (unit) |
| `test_download_connection_failure_preserves_destination_and_removes_temp` | `download::moving_replaces_and_creates_directories`, `download::outputs_into_directories_keep_the_name` |

Old tests with nothing left to cover, by the mechanism they guarded:

- **agent-browser relay** (target filtering, attach and session authorization, backend
  environment scrubbing, guarded backend options): `test_foreign_target_attach…`,
  `test_global_browser_changes_are_refused`, `test_owned_attachment_authorizes_only_returned_session`,
  `test_detach_refuses_foreign_session`, `test_auto_attach_excludes_shared_service_workers`,
  `test_profile_wide_page_commands_are_refused`, `test_target_creation_cannot_select_another_context`,
  `test_cli_cannot_override_guarded_backend`, `test_cli_blocks_global_close_and_nested_execution_bypasses`,
  `test_inherited_provider_profile_and_persistence_are_scrubbed`,
  `test_session_backend_names_do_not_collide_with_generic_sessions`. `hba` has no backend to guard:
  it only issues the protocol calls its own commands need, on the session's tabs.
- **browser-harness daemon and startup pages** (reusing or closing Helium's startup blank page,
  harness daemon provenance, warm re-attachment): `test_first_new_tab_reuses_empty_startup_page`,
  `test_new_tab_preserves_used_page_and_explicit_blank`, `test_modified_startup_document_is_preserved`,
  `test_unused_startup_page_is_closed_after_success_or_error`,
  `test_cleanup_preserves_navigated_startup_and_explicit_blank`,
  `test_failed_reused_startup_navigation_does_not_leak_blank`,
  `test_start_recovers_existing_owned_page_without_another_blank`,
  `test_bootstrap_close_failure_keeps_state_for_retry`,
  `test_daemon_cleanup_requires_creation_provenance_and_blank_url`,
  `test_missing_daemon_provenance_never_closes_unowned_blanks`,
  `test_failed_daemon_cleanup_keeps_provenance_for_explicit_close`,
  `test_cleanup_failure_does_not_hide_the_script_error`, `test_invalid_auxiliary_ownership_fails_closed`,
  `test_detaches_previous_daemon_session_on_warm_attach`,
  `test_warm_dialog_reuses_attachment_without_focus_emulation`, `test_cdp_uses_explicit_owned_session`,
  `test_stale_page_recovery_creates_owned_page`, `test_profile_recovery_uses_requested_url`,
  `test_page_info_unrelated_error_is_not_swallowed`, `test_closing_parent_can_already_have_closed_child`,
  `test_popup_survives_closed_parent_and_is_still_owned`. `hba` never reuses a page it did not
  create; a tab is the session's because the session created or adopted it.
- **tab creation bookkeeping** (`test_creation_registered_before_navigation`,
  `test_failed_navigation_rolls_back_new_page`, `test_failed_registration_rolls_back_new_page`,
  `test_uncertain_creation_does_not_retry`): a new tab joins the session before it navigates, and a
  failed navigation leaves the tab in the session for `hba close`.
- **profile probing** (`test_profile_listing_is_observation_only`, `test_unknown_context_never_borrows_a_user_page`,
  `test_profile_of_does_not_discover_other_profiles`, `test_probe_failure_closes_only_marked_page_and_restores_focus`):
  `hba profiles` reads `Local State` only; contexts are identified with `hba`'s own probe tab, which
  is always closed, and the launcher path restores niri focus.
- **legacy state** (`test_legacy_migration_preserves_owned_ids_and_profile`,
  `test_legacy_missing_binding_retains_evidence`, `test_completed_migration_cannot_resurrect_old_ownership`):
  there is no earlier `hba` state format.
- `test_cli_requires_named_session`, `test_lock_timeout_is_bounded`,
  `test_command_help_does_not_need_a_browser_or_session`,
  `test_endpoint_failure_never_launches_over_existing_browser`,
  `test_absent_browser_launch_uses_requested_profile`, `test_public_endpoint_omits_connection_url`,
  `test_state_is_private_and_atomically_replaced`: see the differences and preserved behavior below.

## Deliberate differences

- **Sessions have a default.** Commands no longer require `--session`: the default is the project
  (git root) directory name, so one agent per project needs no flags. Parallel agents still pass
  distinct `-s` names.
- **One daemon, not one per task.** The daemon keeps the DevTools connection; per-session locks
  keep one session's commands in order (waiting up to 120 s) while sessions run concurrently.
- **The user's tabs can be driven on request.** `hba tab use` adopts a tab; the old tools refused
  every unowned page. Adopted tabs are released, never closed, by `hba close`.
- **A damaged `state.json` is set aside** (`state.json.damaged`) and the store starts empty. The old
  tools refused to run. Nothing is closed on the strength of lost state; tabs it listed just stop
  being owned.
- **The WebSocket URL is shown on request:** `hba browser endpoint` prints the verified URL for other
  DevTools clients such as Jev, and `hba daemon status` shows the one it is connected to. Both are
  local, same-user output; `doctor` and `status` only report that the endpoint was verified.
- **Dropped:** `helium-tab close-browser`, the `--lock-timeout` flag, Python-callable helpers.

## Preserved behavior

- Status and cleanup never launch Helium (`cli::closing_without_a_browser_never_starts_one`), and a
  running Helium whose endpoint fails verification is reported, never relaunched
  (`endpoint::ensure_running`).
- Launching uses the requested profile (`--profile-directory` through the launcher).
- Endpoint verification: loopback only, owned by the Helium using the configured data directory.
- Workspace rules match whole path components; the longest wins.
- Session names: 1–48 characters of `A-Z a-z 0-9 . _ -`.
- Popups opened by session tabs, and their popups, belong to the session.
- `fill` refuses newlines in single-line fields and reports when the field changed the text.
- The state file is written atomically with mode 0600 inside the 0700 runtime directory.

## Verification

```sh
cargo build
cargo clippy --all-targets -- -D warnings      # clean
cargo test                                     # unit and cli tests pass
HBA_LIVE=1 cargo test --test live              # passes against headless Helium 156
```
