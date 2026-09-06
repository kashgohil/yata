# M11.25 closeout — daily-driver integration

This report is the reproducible acceptance record for M11.25. It audits the
M11 parent `2488c83` against the closeout revision without changing `PLAN.md`
or `CLAUDE.md`.

## Milestone summary

M11 integrates live forms and session cookies, table/positioned/grid layout,
document caching, bounded tabs, bookmarks, reader projection, and fresh session
restore through one page-addressed event loop. The closeout reconciles the
choice register, pins the ownership boundaries in one loopback flow, accounts
for the offline ladder, and records fresh integrated performance evidence.

## Deviation disposition ledger

Disposition terms are exactly those required by the task sheet. Test names are
fully qualified only where two modules use similar wording; all are discoverable
with `cargo test <name>`.

| Source | Original claim | Current disposition | Proof |
|---|---|---|---|
| pre-M11 register; M11.1 | `getElementsByTagName` / `getElementsByClassName` absent | **closed since written** — M11.1 implemented snapshot arrays | `get_elements_by_tag_name_walks_in_document_order`; `get_elements_by_class_name_needs_every_class` |
| pre-M11 collections; M11.1 | collections are snapshots and have no `namedItem` | **register** — wording now covers all shipped collection APIs | `mutating_a_collection_while_iterating_it_is_a_snapshot`; `choice_and_select_properties_share_the_live_control_state` |
| M11.1 commit | comma-separated class names return no matches | **not a deviation** — browsers split the argument on ASCII whitespace, not commas | `get_elements_by_class_name_needs_every_class` |
| pre-M11 register; M11.2 | attribute selectors absent | **closed since written** — M11.2 added presence/exact/token/hyphen/substring forms to the shared parser | `style::matching::attribute_selectors`; `queries_go_through_the_css_matcher` |
| M11.2 | selector `i` flag absent | **register** — values remain case-sensitive | `attribute_selector_flags_are_rejected_instead_of_ignored` |
| M11.2 | unquoted attribute values must be CSS identifiers | **not a deviation** — this is CSS selector grammar | `attribute_selector_values_are_identifiers_or_strings` |
| M11.2 | Web Crypto remains absent | **roadmap absence** — no deliberate fallback | danluu `--dump-js`; existing “Not implemented” ownership remains PLAN.md |
| M11.3 | subtree restyle correctness assumes no outward-dependent selector | **register** — sibling combinators/`:has()`/`:target` are the redesign trigger | `mutation_fuzz_keeps_a_scoped_restyle_equal_to_a_full_one`; `the_dynamic_matching_inputs_survive_a_scoped_pass` |
| M11.3 commit details | slice-of-roots API, benchmark flag/victims/CGU placement | **not a deviation** — test seams and internal API/measurement choices are not browser behavior | `style::scoped` tests; `perf.md` M11.3 record |
| M11.4 | assigning `location.hash` reads back next tick | **register** | `location_hash_reads_back_what_a_script_set` |
| M11.4 | unboxed fragment target uses laid-out ancestor | **register** | `a_hidden_target_lands_on_the_nearest_laid_out_ancestor` |
| M11.4 | fragment target resolves once; no `:target` | **register** | `a_fragment_is_resolved_once_and_never_retried`; selector parser tests |
| M11.4 review | entering current URL refetches | **not a deviation** — address-bar Enter and reload refetch in browsers | `the_same_url_without_a_fragment_reloads_rather_than_jumping` |
| pre-M11 register; M11.5 | a script inserted by a script never runs | **closed since written** — direct connected insertion runs in a later turn | `an_inserted_inline_script_runs_in_a_later_turn_and_reaches_the_screen` |
| M11.5 | inserted subtrees, `innerHTML`, and template clones remain inert | **register** | `a_script_inserted_into_a_template_or_noscript_never_runs`; `a_script_written_by_inner_html_never_runs` |
| M11.5 | inserted scripts run when ready and do not delay document load | **register** | `an_inserted_script_runs_although_a_document_slot_is_still_in_flight`; `dom_content_loaded_and_load_fire_once_although_an_insertion_finishes_the_queue_twice` |
| pre-M11 event list; M11.5 | only click/document lifecycle events exist | **closed since written** in that broad form — inserted-script error/load and M11.13 form events landed | `an_inserted_external_script_runs_then_fires_load_at_its_element`; `keyboard_and_mouse_choice_activation_dispatches_native_events` |
| M11.5 | reflected `script.src` stays unresolved | **register** | `script_properties_reflect_their_attributes`; request URL assertions in inserted-script app tests |
| M11.5 | missing general Node/CSSOM members, except `parentNode` | **register** — old `parentNode` absence is **closed since written** | `parent_node_reaches_the_document_where_parent_element_stops`; binding absence tests |
| M11.6 | `document.cookie` absent | **closed since written** | `a_script_reads_back_the_cookies_it_sets` |
| M11.6 | cookies are process-memory-only and `Domain` is host-only | **register** | `nothing_a_cookie_knows_outlives_the_process`; `a_cookie_never_crosses_a_host_not_even_to_a_subdomain` |
| M11.6 | no cookies on wire / ignored credentials | **closed since written** — M11.7 carries response/request cookies and honors omit | `a_server_set_cookie_reaches_the_jar_and_the_page_sees_its_half`; `fetch_honours_credentials_and_omit_is_the_only_one_that_means_anything` |
| M11.6 | HTTP and HTTPS on one host share non-Secure cookies | **not a deviation** — cookie scope is host/path and `Secure`, not origin | `a_secure_cookie_is_neither_set_nor_read_over_http` |
| M11.6 | SameSite stored but unused | **roadmap absence** — no cross-site credentialed request exists to apply it to | cookie parser tests; same-origin fetch guard |
| M11.7 | redirect hop loses Set-Cookie and reuses initial cookie header | **closed since written** — M11.7a routes document hops through the loop | `a_hop_sets_the_session_and_the_next_request_carries_it`; `a_hops_cookie_is_scoped_to_the_hop_not_to_where_the_chain_ends` |
| M11.7/M11.7a | subresource response/hop cookies and mixed-scheme origin boundary | **register** | `a_subresource_response_has_nowhere_to_put_a_set_cookie`; `a_subresource_redirect_to_another_host_still_drops_the_cookie`; `a_pages_subresources_carry_its_cookies_and_a_cross_origin_one_does_not` |
| M11.7 | `include` equals same-origin; `omit` omits | **register** as part of the bounded credentials model | `fetch_honours_credentials_and_omit_is_the_only_one_that_means_anything` |
| M11.7 | page-authored Cookie header is stripped | **not a deviation** — Cookie is a forbidden request header in browsers | `a_page_cannot_write_its_own_cookie_header_through_fetch` |
| M11.7a | document hops use fresh connections | **register** | `net::fetch::a_redirect_is_a_message_and_the_worker_stops_there`; M11.7a loopback measurement |
| M11.7a | no Refresh/meta-refresh | **roadmap absence** — unsupported mechanism with no fallback | redirect-target tests |
| M11.7a | all redirect statuses preserved the method | **closed since written** — M11.11 rewrites 301/302/303 and preserves 307/308 | `post_redirects_rewrite_the_method` |
| M11.7a | redirect chain is one history entry | **not a deviation** — browser redirect chains do not create reader history entries per hop | `a_redirect_chain_is_one_history_entry` |
| M11.8 | checkbox/radio/select render nothing and controls have no live properties | **closed since written** — M11.12/M11.13 landed the scoped controls and properties | `clicks_activate_a_checkbox_but_only_focus_a_select`; `choice_and_select_properties_share_the_live_control_state` |
| M11.8 | specialized/file types, padding-drawn frame, empty icon button, bounded size attributes | **register** for observable terminal fallbacks | field/layout tests including `select_size_and_unicode_width_follow_cell_defaults_and_bounds`; snapshots |
| M11.8 | search/resize anchors ignore control value boxes | **register** — values are UI state, not page prose/anchor candidates | `a_textareas_value_is_a_value_and_never_page_prose`; viewport anchor tests |
| M11.9 | textarea Enter, no rich editing, click swallowed in Field mode | **register** | `a_textarea_types_line_by_line_and_enter_never_inserts_a_newline`; `a_click_while_typing_submits_nothing_and_navigates_nowhere` |
| M11.9/M11.10 | PLAN key table lacks Field/Enter rows | **roadmap absence** from the choice register; retained as a proposed documentation correction below | `help_lists_the_field_mode_because_it_is_generated_from_the_table`; `enter_in_a_field_submits_the_form` |
| M11.10 | one-field rule ignored; implicit default submitter omitted; invalid method refused | **register** | `a_two_field_button_less_form_submits_on_enter`; `the_activating_button_is_the_only_button_in_the_set`; invalid-method form tests |
| M11.10 | POST does not navigate | **closed since written** — M11.11 sends POST | `a_login_shaped_post_hops_to_get_with_the_cookie` |
| M11.10–M11.12 | reader changes dispatch no form events and cannot cancel submit | **closed since written** — M11.13 owns native events | `a_submit_listener_can_cancel_field_submission`; `tab_commits_text_before_blur_and_next_focus`; `select_events_follow_changed_commits_and_multiple_toggles_only` |
| M11.10–M11.12 | reset, `form=""`, labels, disabled fieldset, file/specialized controls, select mode | **register** | `a_button_that_is_not_a_submit_button_submits_nothing_and_resets_nothing`; form owner and select-mode tests |
| M11.11 | reload/forward never replay POST; encoding/submitter override/invalid method rules | **register** | `reload_after_a_post_200_is_a_get_with_no_body`; `dialog_and_multipart_still_refuse`; `a_method_this_engine_cannot_send_is_refused_by_name` |
| M11.11 | 301/302 POST-to-GET | **not a deviation** — established browser behavior; 303/307/308 also match implemented rules | `post_redirects_rewrite_the_method` |
| M11.12/M11.13 | no stateful CSS pseudo-classes | **register** | selector pseudo-class tests; live state remains outside attributes |
| M11.13 | snapshot selectedOptions, small selectedIndex coercion, absent submitter/programmatic activation/handlers, teardown events | **register** | `choice_and_select_properties_share_the_live_control_state`; event/binding tests and absence paths |
| M11.14 provisional table model | max-intrinsic columns; no spans/border model | **closed since written** — M11.15/M11.16 added auto columns, spans, and shared edges | `table_spans_claim_rectangles_and_leave_no_interior_grid_rule`; `table_columns_keep_rank_and_vote_compact_while_title_spends_the_width` |
| M11.14–M11.16 final table limitations | DOM roles/HTML spans and remaining CSS/parser/accessibility limits | **register** with M11.16's narrowed wording | `tables.html`; table layout and CLI dump tests |
| M11.17 | fixed/sticky absent | **closed since written** — M11.18 implemented bounded fixed/sticky | `scrolling_fixed_and_sticky_output_uses_the_cached_layout` |
| M11.17 final physical inset/containing-block/sizing/stacking limits | **register** | `positioned_boxes_keep_flow_and_use_final_rectangles`; positioned fixture/CLI tests |
| M11.18 | grid absent as containing/layout context | **closed since written** for layout — M11.19 added grid; grid still is not an implicit absolute containing block by policy | grid layout tests; positioned containing-block tests |
| M11.18 | terminal viewport and start-edge document-sticky boundaries | **register** | paint fixed/sticky tests; `scrolling_fixed_and_sticky_output_uses_the_cached_layout` |
| M11.19 | explicit row-major terminal grid and missing full algorithms/features | **register** | `grid_tracks_repeat_minmax_and_placements_are_bounded`; grid fixture/goldens |
| pre-M11 “Not implemented” | tables, absolute/fixed/sticky, grid blockify/stay in flow | **closed since written** — removed from absence inventory | table/position/grid fixture and dump tests |
| M11.20 | bounded private memory document cache and exact freshness/validator/Vary subset | **register** | `http_cache` unit matrix; `loopback_a_b_back_skips_a_network_request_and_reload_validates` |
| M11.20 | excluded response classes, no stale-on-error/BFCache | **register** | `post_and_set_cookie_responses_are_never_inserted`; `stale_unvalidated_response_does_a_full_get_and_failure_never_serves_stale`; fresh-host cache tests |
| M11.21 | bounded shared-resource/live-state tab model and background/late-result/key-only boundaries | **register** | `async_messages_route_by_tab_before_the_generation_guard`; `cookies_and_document_cache_are_shared_without_sharing_page_state`; tab operation tests |
| M11.21 | tab set/order memory-only | **closed since written** — M11.24 persists order/current URL/active ordinal | `untouched_startup_restores_order_with_fresh_ids_and_addressed_work` |
| M11.22 | bounded private bookmark list/file, coordination/interchange/organization/title limits | **register** | bookmark codec/worker bounds; `bookmarks_load_mutate_globally_and_open_through_active_history` |
| M11.22 | tabs remain in memory | **closed since written** — M11.24 session recipe | session restore tests |
| M11.23 incomplete-contract checklist | extraction/projection/integration/counter promises might require deviations | **not a deviation** — all checklist acceptance landed; register only describes what the shipped product claims differently | reader analyzer/app/snapshot/hostile/performance tests |
| M11.23 | projection is not general extraction/sanitization/offline/script isolation | **register** — only where compared with richer reader products | `reader_toggle_is_ua_only_exact_once_and_restores_normal_page`; hostile reader tests |
| M11.24 | bounded fresh-navigation recipe omits live/history/credential/cache/reader/form state | **register** | `restore_is_a_fresh_context_and_load_ack_run_no_page_stage`; `reader_scrolling_checkpoints_the_retained_normal_row_and_restores_normal_mode` |
| M11.24 | startup arbitration, private/coalesced format, abrupt/multiprocess boundary | **register** | `cli_and_early_session_mutations_win_but_chrome_only_actions_do_not`; session worker/codec tests |

No ledger candidate is **unproven** after the M11.25 integration and audit
tests below. Roadmap absences remain outside the deliberate-choice register,
except for the established compact “Not implemented at all” inventory.
