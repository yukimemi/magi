# CLI integration-test isolation

Use `common::magi_cli(home)` to spawn the real binary. The caller keeps its
fixture directory alive until the child exits. The helper sets `MAGI_HOME`,
`MAGI_CONFIG_DIR`, `MAGI_NO_AUTOUPDATE=1`, and a temporary cwd, and removes
inherited `MAGI_RUN`, `MAGI_NODE`, `MAGI_SEAT`, and `MAGI_TURN_SCOPE`. Tests may
explicitly override these defaults afterward.

Agent and shell adapters copy the same defaults while preserving the fresh
seat identity assigned by the launcher and its explicit fixture cwd. The
`update_exit` test deliberately re-enables updates with its existing fake `gh`,
private HOME/cache/configuration, token removal, and process-group cleanup.
The source audit permits the binary reference only inside `magi_cli`, including
when scanning nested test modules.

## Local timing comparison

Linux measurements could not be collected: the available local runner is macOS
and has no available Linux container runtime. The table below reports **macOS**
measurements instead; Linux before/after timings remain a follow-up.
**Windows numbers must be read from the next CI run's JUnit artifacts.**

The baseline is `b301cab9f906ccd60cb6fa957292ae47464a0ef9`. Both versions used
Rust stable (`rustc 1.98.1`) and the shared build cache. After building, each of
the 57 tests ran once, sequentially, as a separate libtest process with
`--exact --test-threads=1`. Wall-clock seconds include process startup and exit;
build time is excluded. No other build or test invocation from this task ran
during either recorded measurement.

Both runs inherited `MAGI_NO_AUTOUPDATE=1`, temporary HOME, cache, configuration
and magi directories, a temporary initial cwd, and no seat identity variables.
This safe baseline intentionally does **not** reproduce the old GitHub request
or shared-cache access, so these single samples do not measure the removed
network delay or establish a performance improvement.

| Test | Before (s) | After (s) |
| --- | ---: | ---: |
| `a_chat_merge_from_a_different_chat_is_refused` | 1.603 | 1.529 |
| `a_chat_merge_quoting_the_owner_is_accepted` | 0.043 | 0.098 |
| `a_chat_merge_with_words_the_owner_never_wrote_is_refused` | 0.041 | 0.109 |
| `a_chat_merge_without_a_quote_is_refused` | 0.041 | 0.107 |
| `a_direct_owner_hold_does_not_inherit_conditions_from_assistant_prose` | 0.046 | 0.094 |
| `a_same_chat_batch_is_not_a_specific_merge_confirmation` | 0.044 | 0.103 |
| `ambiguous_short_ids_and_conflicting_recommendations_do_not_bind_a_batch` | 0.079 | 0.192 |
| `ask_chat_busy_exits_unsuccessfully_without_changing_state` | 0.057 | 0.137 |
| `batch_holds_reject_entire_ambiguous_or_conditional_recommendations` | 0.319 | 0.726 |
| `batch_holds_validate_surrounding_headings_and_paragraphs` | 0.413 | 0.890 |
| `batch_reports_action_specific_missing_consent_in_every_conversation` | 0.233 | 0.495 |
| `chat_merge_preserves_the_recorded_head_confirmation_requirement` | 0.101 | 0.217 |
| `cli_reloads_a_concurrently_settled_question_after_evidence_contention` | 0.275 | 0.395 |
| `cli_waits_for_the_conversation_evidence_lock_before_reading_owner_intent` | 0.295 | 0.307 |
| `competing_non_hold_instructions_have_one_winner_and_no_second_execution` | 0.197 | 0.101 |
| `concurrent_standing_answers_settle_once_and_keep_the_winning_authority` | 0.192 | 0.098 |
| `consult_other_is_a_note_without_answer_or_choice_effect_and_invocation_stays_restricted` | 0.093 | 0.164 |
| `consult_task_add_binds_current_owner_operation_and_execution_arguments` | 0.202 | 0.380 |
| `consult_task_add_rechecks_owner_evidence_after_async_preparation` | 0.444 | 0.355 |
| `consult_task_evidence_rejects_wrong_target_predelivery_queued_and_generated_sends` | 0.111 | 0.217 |
| `consult_task_hold_priority_and_done_keep_queue_claims_and_exact_run_grants` | 0.127 | 0.201 |
| `consultation_waiting_for_evidence_cannot_undo_a_manual_cli_hold` | 0.163 | 0.242 |
| `cross_chat_merge_and_closure_require_individual_bound_tokens` | 0.149 | 0.232 |
| `deputy_replacement_title_rejects_hold_retraction_and_quoted_or_missing_consent` | 0.433 | 0.852 |
| `deputy_replacement_title_requires_current_content_binding_and_publishes_only_literal_text` | 0.119 | 0.197 |
| `explicit_batch_binds_each_preceding_recommendation_without_approving_merge` | 0.130 | 0.231 |
| `explicit_public_content_and_literal_owner_title_are_bound_to_the_question` | 0.169 | 0.251 |
| `explicitly_bound_cross_chat_and_unconsulted_holds_record_owner_evidence` | 0.237 | 0.425 |
| `generated_and_quoted_recommendations_do_not_authorize_but_hold_is_separate_from_closure` | 0.309 | 0.494 |
| `generated_sends_do_not_replace_the_latest_real_owner_instruction` | 0.253 | 0.329 |
| `guarded_quote_whitespace_keeps_legacy_comparison_and_complete_owner_evidence` | 0.361 | 0.491 |
| `japanese_batch_dismisses_obsolete_execution_without_touching_its_pr` | 0.220 | 0.330 |
| `legacy_consult_repair_requires_exact_owner_choice_and_keeps_original_handoff` | 0.137 | 0.151 |
| `legacy_recorded_send_boundaries_support_existing_owner_authorization` | 0.128 | 0.215 |
| `non_hold_batch_decisions_use_current_manifests_across_consultation_destinations` | 1.225 | 1.414 |
| `non_hold_manifests_reject_changed_questions_recommendations_and_busy_consultations` | 0.303 | 0.370 |
| `operator_held_chat_approvals_still_refuse_even_explicit_holds` | 0.116 | 0.183 |
| `operator_held_tasks_keep_ordinary_chat_decisions_available_without_lifting_the_hold` | 1.843 | 2.552 |
| `ordinary_chat_task_add_does_not_require_an_existing_talk_record` | 0.169 | 0.276 |
| `owner_code_examples_do_not_authorize_non_hold_actions` | 0.219 | 0.306 |
| `recovery_cli_accepts_japanese_hold_and_diagnoses_refused_bindings` | 0.257 | 0.367 |
| `recovery_cli_accepts_short_and_full_ids_with_stable_aliases_and_localized_labels` | 2.585 | 3.659 |
| `recovery_cli_does_not_extend_merge_posting_or_destructive_consent` | 0.500 | 0.555 |
| `recovery_cli_does_not_reconstruct_old_bindings_or_accept_alias_collisions` | 0.354 | 0.377 |
| `recovery_cli_generated_sends_in_the_owner_turn_do_not_replace_real_evidence` | 0.067 | 0.123 |
| `recovery_cli_interprets_natural_japanese_with_current_persisted_recommendation` | 0.304 | 0.345 |
| `recovery_cli_preserves_pending_merge_approval_and_existing_run_ownership` | 0.195 | 0.146 |
| `recovery_cli_rejects_stale_ambiguous_missing_and_mismatched_evidence` | 1.069 | 0.736 |
| `recovery_quote_whitespace_still_requires_the_exact_complete_owner_send` | 0.289 | 0.176 |
| `recovery_recommendation_uses_existing_question_grants_and_excludes_legacy_requeue` | 0.272 | 0.209 |
| `same_chat_replacement_title_cannot_be_authorized_by_hold_or_old_thread_text` | 0.267 | 0.242 |
| `same_chat_replacement_title_uses_bound_current_owner_text_and_retains_evidence` | 0.085 | 0.079 |
| `same_chat_single_word_hold_remains_supported` | 0.088 | 0.111 |
| `standing_evidence_requires_real_sends_fresh_questions_and_current_instructions` | 0.731 | 0.930 |
| `standing_holds_reject_missing_ambiguous_conditional_and_retracted_intent` | 0.683 | 0.989 |
| `unconsulted_free_text_uses_the_complete_bound_owner_answer` | 0.076 | 0.099 |
| `use_my_text_never_reuses_old_thread_content_without_a_new_literal_title` | 0.066 | 0.121 |
| **Sum** | **19.527** | **25.720** |
