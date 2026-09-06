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

## Offline ladder sweep

Both revisions were built with `cargo build --release`. The same
`python3 -m http.server 18765 --bind 127.0.0.1` process served the candidate's
committed fixtures, and each binary ran `--dump-text`, `--dump-boxes`,
`--dump-js`, and `--timing` against the same URL. Captures were compared with
`cmp` and `diff -u`; they are intentionally not committed because they contain
the loopback port and duplicate deterministic checked-in fixtures/goldens.

| Page at 80 cells | Baseline `2488c83` | M11 result | Responsible rule | Committed evidence |
|---|---|---|---|---|
| example.com | text, boxes and JS byte-identical | byte-identical | no M11 surface is present | `layout::ladder::example_com`; `snapshots::example_com` |
| motherfuckingwebsite.com | text/boxes identical; analytics loader stopped at missing collection/parent surface | text/boxes identical; inline loader and inserted analytics script complete | M11.1 collections + M11.5 `parentNode`/inserted scripts; closer to browser execution | `the_analytics_shape_that_started_this_task_now_runs`; `a_script_a_script_inserted_reaches_a_headless_dump` |
| danluu.com | text/boxes identical; beacon stopped on `[data-cf-beacon]` parse | text/boxes identical; beacon reaches the later missing Web Crypto wall | M11.2 shared attribute selector grammar; closer to browser execution | attribute-selector matcher matrix; `query_selector_all_on_the_wikipedia_fixture` |
| news.ycombinator.com | tables blockified into 146 rows; ranks/titles/metadata did not share columns | 117-row table grid; rank/vote/title columns and metadata align; JS identical | M11.14–M11.16 DOM table roles, automatic columns, spans/shared edges; closer to browser structure | `tables.boxes`; `table_columns_keep_rank_and_vote_compact_while_title_spends_the_width`; CLI table dumps |
| en.wikipedia.org | controls were plain/absent text and inline script stopped on a missing DOM surface | fields/buttons/choices have explicit boxes; inline script advances; article prose remains present | M11.8–M11.13 control boxes/live state and M11.1/M11.5 DOM surface; documented terminal widget policy, otherwise closer to browser behavior | `wikipedia-search.txt`; `wikipedia-choice.txt`; Wikipedia layout/style ladder tests |

No core page loses prose in `--dump-text`. HN's height reduction is the removal
of blockified cell rows, not dropped stories; Wikipedia's height reduction is
accounted for by control/table/positioned layout and its text dump retains the
article. The only JavaScript stops remaining in the changed dumps are explicit
roadmap absences (Web Crypto or another unimplemented DOM/Web API), not early
regressions in a shipped M11 surface.

Candidate-only page-shaped coverage was swept through the same four headless
modes:

| Fixture | Result and consistency evidence |
|---|---|
| `js.html` | text/boxes/JS deterministic; headless inserted-script tests use the same adoption/event loop |
| `form-events.html` and choice goldens | values painted in fields are the values serialized and observed by focus/input/change/submit listeners; CLI and app tests agree |
| `tables.html` | text retains every cell; boxes label table/row/cell and final spans; rendered tests consume the same rectangles |
| `positioned.html` | text remains in DOM order; boxes report final relative/absolute geometry; hit tests use those rectangles |
| `fixed-sticky.html` | static text remains present, boxes annotate viewport/sticky constraints, and paint/hit tests apply the cached scroll adjustment |
| `reader-mode.html` | normal DOM/boxes remain intact and the two committed rendered projections account for reader pruning/styling |
| `m11-integration.html` | grid/table/sticky/absolute roles are asserted before interaction; `m11-integration-settled.txt` is the stable two-tab reader/search frame |

Exact demonstration commands:

```text
python3 -m http.server 18765 --bind 127.0.0.1
for page in example.com motherfuckingwebsite.com danluu.com news.ycombinator.com en.wikipedia.org; do
  target/release/yata --dump-text http://127.0.0.1:18765/tests/fixtures/$page.html
  target/release/yata --dump-boxes http://127.0.0.1:18765/tests/fixtures/$page.html
  target/release/yata --dump-js http://127.0.0.1:18765/tests/fixtures/$page.html
  target/release/yata --timing http://127.0.0.1:18765/tests/fixtures/$page.html
done
cargo run --release -- http://127.0.0.1:18765/tests/fixtures/m11-integration.html
cargo test --lib m11_daily_driver_boundaries_hold_in_one_loopback_session_and_restart
```

## Spec-golden and acceptance-evidence audit

Every `.boxes` and rendered snapshot introduced since `2488c83` was walked
with `git log --follow`. No pre-M11 golden was rewritten. The derivations are:
M11.8 field geometry (`form-fields`, `form-controls*`, HN/Wikipedia search),
M11.12 choice normalization/mode (`choice-*`, Wikipedia choice), M11.14–16
DOM-table tracks/spans/shared edges (`table-*`, `tables.boxes`), M11.17–19 final
position/fixed/sticky/grid rectangles (`positioned`, `fixed-sticky*`, `grid*`),
M11.23 live reader projection (`reader-mode*`), and M11.25's integrated settled
frame. Their named commits are retained by `git log --follow`; none was moved
without a derivation change.

| Task | Current acceptance evidence and hostile/degenerate bound |
|---|---|
| M11.1 collections | binding tests cover document/element scope, `*`, multi-class intersection, order and snapshot mutation; ladder analytics progresses |
| M11.2 attribute selectors | parser/matcher matrices cover six operators, empty values, compounds, specificity, malformed syntax, case-sensitive values and rejected flags; DOM queries reuse it |
| M11.3 scoped restyle | all five ladder oracles, seeded mutations, overlapping roots, dynamic hover/link/visited inputs, detached/grown/over-cap fallback and exact counters |
| M11.4 fragments | resolver table, percent-decode hostility, Wikipedia citation round trip, hidden/unboxed fallback, once-only resolution, redirect/resize/history anchors and flat stage counters |
| M11.5 dynamic scripts | queue ordering/holes, direct vs subtree/template/innerHTML/unconnected/moved cases, 32-link hostile chain, later-turn events, headless parity and exact invalidation counters |
| M11.6 cookies | parser/attribute/path/secure/httpOnly/expiry matrices use injected clocks; 4 KiB/50-host bounds, replacement/refusal, process lifetime and no-stage script writes |
| M11.7 wire cookies | request-header and Set-Cookie seam tests, same/cross-origin credentials, mixed scheme, malformed/non-UTF8 lines, forbidden Cookie header, subresource boundary and loopback login |
| M11.7a redirects | every status, missing/invalid Location, 20-hop loop, hop cookie/path/host recomputation, same PageId, stale generation, fragments/history/timing/headless parity |
| M11.8 fields | value/default/placeholder/password/readonly/disabled/type coercion, Unicode/size caps, flex/inline/block layout, resize anchors, focus/hit/CLI/snapshots and one-pass counters |
| M11.9 typing | cell-aware caret/edit/delete/home/end/tab/Esc/Ctrl-C, CJK, textarea scroll and no-newline rule; listener isolation and keystroke measurements |
| M11.10 GET forms | owner/successful-control/tree-order/hidden/query/CRLF/encoding tables, HN/Wikipedia activators, no-name/empty set, one-field and default-submitter differences |
| M11.11 POST forms | 1 MiB body edge, enctype/method refusal, submitter policy, wire body/content type/cookie, POST redirect matrix, reload/history non-replay and no-op counters |
| M11.12 choices | default/dirty checked/selected state, radio groups, disabled option/group, multiple/listbox bounds, glyph/layout/focus/hit/submit and hostile DOM repair |
| M11.13 form events | focus/blur/input/change/click/submit order/cancel/mutation/navigation, live properties/coercion/snapshot selectedOptions, action revalidation and exact-once counters |
| M11.14–16 tables | role/fallback fixtures, narrow/one-cell/CJK/deep/absurd widths and spans, occupancy/no-overlap, HN constraints, nested/flex/fields/images, border clipping and one-pass/scroll counters |
| M11.17–18 positioning | cascade/insets/containing blocks, out-of-flow contexts, impossible sizes/offsets, final paint/hit/search/inspector geometry, viewport/fixed/sticky bounds and scroll/resize counters |
| M11.19 grid | grammar caps and hostile sums, one-cell/CJK/long words, explicit/spans/sparse implicit rows, nested formatting contexts, overlap/clip/hit/focus/search/inspectors and scroll counters |
| M11.20 cache | byte/entry/LRU/metadata/Vary bounds, freshness/validator clock, exclusions, cookie keying, stale failure, 304/200 replacement, fresh host state, loopback back/reload and headless isolation |
| M11.21 tabs | 16-tab/id bounds, ordered wrap/close replacement, page-address-before-generation routing, interleaved chains, close/late work, shared vs local state, deferred resize/Kitty and flat switch counters |
| M11.22 bookmarks | 1,024/URL/title/file/codec bounds, corrupt/future/read-only files, atomic failure points, worker latest-wins races, independent startup order, active-tab open and headless isolation |
| M11.23 reader | bounded candidate/scoring/pruning and deep/large hostile inputs, author-hidden/UA-inert reveal, immutable DOM, mutation/focus/search/fragment/tab/bookmark/inspector/image/timer integration, snapshots and exact counters |
| M11.24 restore | 16-tab/URL/file/scroll codec bounds, manual-clock coalescing, permissions/atomic failures/load races/stale acks, startup permutations, fresh identities/navigation, background routing and omitted live state |
| M11.25 integration | real loopback POST→cookie redirect→same-origin request, cache-hit bookmark open, page-address assertions on every async message, independent injected workers, graceful flush/fresh restore, and stable frame |

Headless modes construct no bookmark/session workers and all checked-in CLI
tests use one-shot loopback servers. No task promise relies on the public
network, a sleep, an ephemeral path in a golden, or a timing value in a
snapshot. Stage-counter coverage includes refused/no-op actions, cookie-only
ticks, fragment/scroll, cache hits, tab switches, bookmark chrome, persistence
messages, reader toggles, and table/grid/position resize and scroll paths.

## Cross-feature state and race audit

| Boundary | Current evidence | Result |
|---|---|---|
| cache + cookies + POST/redirect metadata | `post_and_set_cookie_responses_are_never_inserted`, `a_logged_out_entry_cannot_satisfy_the_same_url_after_login`, `post_redirects_rewrite_the_method`, M11 integration loopback | POST/hops are not cached; every continuation recomputes the right cookie header and request method |
| tab close/switch during document and subresource work | `async_messages_route_by_tab_before_the_generation_guard`, `interleaved_document_chains_settle_in_their_addressed_tabs`, `a_response_arriving_after_navigation_is_dropped`, background sheet/image/script/cache tests | every page result is addressed by `(TabId,generation)`; a closed identity is never reused or resurrected |
| bookmark/session worker ordering and revisions | `bookmark_and_document_startup_messages_are_independent_in_both_orders`, bookmark latest-wins/failure tests, `stale_session_acknowledgements_never_mark_a_newer_snapshot_saved` | worker acknowledgements neither overwrite newer memory nor block each other/UI |
| startup arbitration | `cli_and_early_session_mutations_win_but_chrome_only_actions_do_not`, `restored_offset_beats_fragment_once_and_checkpoints_while_loading`, scroll/reload/tab/bookmark/quit checkpoint tests | session-before/after CLI URL and first meaningful mutation permutations are explicit |
| reader/tab isolation | `reader_state_is_per_tab_and_bookmark_modal_preserves_it_without_work`, `reader_search_and_normal_scroll_survive_both_toggles`, `excluded_image_decode_and_reader_inspectors_obey_the_projection` | projection styles/focus/search/scroll/frame are tab-local; session cache/title remain normal-page data |
| anchors across new layout modes | table/grid/position resize+scroll tests, `sticky_grid_heading_uses_cached_static_space_at_several_scroll_offsets`, reader fragment/history tests | fragment/history/resize anchors survive final rectangles and projection changes |
| active-presentation interactions | grid/flex/table hit/focus/hint/search suites; select repair tests; reader interaction/inspector tests | hints, hover, clicks, search, modes and inspectors query the currently visible active presentation |

The M11 integration helper additionally asserts that every asynchronous page
message names a live tab and its current generation before applying it. That
turns the most dangerous equal-generation cross-tab race into a deterministic
failure rather than a visual symptom.

## Fresh M11 performance summary

Machine A is an Apple M4 Pro running macOS 26 (Darwin 25.5), rustc 1.96.1, on
2026-09-06. Parent `2488c83` and candidate release binaries alternated first
place for seven 80×24 loopback rounds; round zero was discarded. Full commands,
sample spreads, memory shapes and operation definitions are in the final
`perf.md` section.

| Gate | Parent | Candidate | Disposition |
|---|---:|---:|---|
| danluu fetch / engine | 3.10 / 1.25 ms | 3.08 / 1.45 ms | 4.53 ms combined; passes 50 ms |
| HN fetch / engine | 2.98 / 1.15 ms | 2.90 / 1.42 ms | table pipeline is present; 4.32 ms combined |
| Wikipedia fetch / engine | 3.27 / 66.83 ms | 3.40 / 71.32 ms | 74.72 ms combined; passes 250 ms |
| field / checkbox / select → frame | no equivalent feature | 3.11 ms / 2.75 ms / 23.00 µs | all pass 10 ms; stages `(0,0,1)` |
| tab / bookmark navigation / reader enter p95 | no equivalent feature | 24.776 µs / 64.333 µs / 5.276 ms | all pass 10 ms |
| ordinary / fixed-sticky scroll | feature differs | 6.006 / 6.806 µs | style/layout flat; passes 5 ms |
| cache hit / validator 304 | no parent cache | 41.750 / 433.125 µs | deterministic loopback observations |
| POST + cookie 302 + article | no parent POST equivalent | 1.646 ms | deterministic loopback observation |
| bookmark decode/apply | no parent persistence equivalent | 25.616 ms worker / 3.939 µs UI | disk/decode stays off input thread |
| session decode/apply/final flush | no parent restore equivalent | 485.935 µs worker / 16.476 µs UI / 10.580 ms shutdown | UI work passes; disk/join separately reported |
| one Wikipedia tab memory | — | 90,210,304-byte max RSS | passes 100 MB |
| integrated 16-tab/product-state shape | — | 90,472,448-byte max RSS | +262,144 bytes; not mislabeled as one-page budget |
| settled idle | — | sleeping, 0.0% CPU | page/cache/workers quiet; pending-timer condvar test green |

The first closeout run caught full-layout field and checkbox turns at 11.30 ms
and 10.73 ms. The fixed-geometry control patch already had a same-tree oracle;
M11.25 shipped it for live text and choice state. Fresh candidate values are
inside budget, while the ignored benchmark retains the old path as a same-
process baseline.

## Interactive inspectors and product feel

Manual gate: real `target/release/yata`, 80×24 Terminal PTY, isolated
`YATA_BOOKMARKS_PATH`/`YATA_SESSION_PATH`, and a loopback-only server. Pages
used were the five core ladder fixtures, `m11-integration.html`, and its
POST-redirect article/member responses.

- F1 showed the live form DOM and, while reader mode was active, the unchanged
  article DOM rather than a rewritten projection.
- F2 exposed sticky/relative/absolute and grid track values on the integration
  page and the final flex state on danluu. F3 labeled `table-row`, `table-cell`
  and final rectangles on HN and the integration page. Reader author/UA style
  separation and wrong-tab inspector isolation were also checked against their
  deterministic frame/unit assertions rather than inferred from overlay text.
- F4 showed fetch, parse, style, layout, script and frame on the fresh fixture.
  Cache/restore stage accounting was cross-checked against the loopback counter
  tests so transient scheduling was not inferred from a screenshot.
- F5 showed exactly one `error deliberate integration fixture probe`; the
  integration test pins the same one-entry/error count. Article/member tabs had
  their own empty consoles, and switching did not duplicate or leak the probe.

The hand-run form sent `q=terminal&daily=yes&topic=rust&send=1`; its submit
listener cookie was present on the POST, the 302 added `sid=manual`, and both
cookies reached `/article` and `/member`. Back/forward returned normally,
scrolling retained sticky chrome, a bookmark opened the article in the second
tab, reader search found `needle`, and F1 remained usable in reader mode. A
normal quit restored the terminal, joined both workers, and a no-CLI relaunch
restored two tabs in order with the second active through two fresh requests;
both requests correctly carried no prior-process cookie. `q` worked from the
reader inspector, and the deliberately exercised HTTP-404 exit also restored
the terminal. Settled `top` reported `sleeping`, 0.0% CPU. Progress/status modes
were distinct; the local bodies completed below 100 ms, so no >100 ms progress
sample was manufactured.

## Proposed PLAN.md and CLAUDE.md patch (not applied)

| File/section | Current claim | Exact replacement | M11 evidence |
|---|---|---|---|
| `PLAN.md` §2 event-loop diagram/text | only input, fetch and “timers (later)” feed the channel; fetch ends at Loaded/Error | diagram producers become `input; document/style/image/script/fetch workers; timer scheduler; bookmark worker; session worker → one mpsc → page-addressed update → render`; text adds cache planning/cached parse, same-generation redirect continuations, browser-global persistence messages, and graceful joins | `main.rs::apply_batch`; `DocumentWork`; redirect/cache/tab/worker integration tests |
| `PLAN.md` §2 pipeline | static fetch→parse pipeline and dirty flags imply navigation-only rebuilds | retain the transform diagram, then add: “JS/event turns invalidate once; live fixed-size controls may patch paint only; cached/restored documents re-enter at parse; reader mode lays out a UA projection without mutating DOM.” | invalidation counters, control tree oracle, cache and reader tests |
| `PLAN.md` §3 key table | link-only Tab/Enter and `F1`–`F4`; no M11 chrome/modes | replace with generated-registry rows for Field (`Enter` submit; Esc; edit/caret; Tab), Select (arrows/Home/End/Space/Enter/Esc/Tab), `t`/`x`/`gt`/`gT`, `a` add bookmark and `b` library, `R` reader, and `F1`–`F5` DOM/styles/boxes/timing/console. There is no shipped `B` binding; the task's suggested `b`/`B` pair must not override the registry. | `keys::BINDINGS`; generated help tests; manual gate |
| `PLAN.md` §5 dependencies/gates | generic unversioned M10 engine choice; inspectors stop at F4 | allowed installed list becomes `crossterm 0.29, reqwest 0.13 blocking+gzip, unicode-width 0.2, image 0.25 scoped codecs, rquickjs 0.12`; merge gate says F1–F5, headless modes, loopback integration and persistence shutdown | `Cargo.toml`; full closeout commands |
| `PLAN.md` §6 M10 | all JavaScript checkboxes remain unchecked | mark the shipped document-order bindings/events/timers/invalidation/budget/fetch-storage-location bullets complete and link the M10 closeout evidence; retain only genuinely unshipped follow-ups | M10 tests and `perf.md`; current F5/worker behavior |
| `PLAN.md` §6 M11+ | forms through reader mode are still one undifferentiated future-interest sentence; restore absent | replace with a completed M11 summary: cookies/forms/events, tables/position/grid, document cache, bounded tabs/bookmarks, reader projection and fresh session recipe; leave incremental relayout, streaming parse and video under “remaining horizon” | M11.1–M11.25 evidence table above |
| `CLAUDE.md` hard rule 4 / definition of done | inspectors are F1–F4 | both occurrences become “F1 DOM, F2 styles, F3 boxes, F4 timing, F5 per-page JS console”; DOD also requires deterministic headless and active-tab inspector isolation | console fixture probe; inspector/tab/reader tests |
| `CLAUDE.md` architecture/event wording | every worker `Msg` is implicitly page work; all I/O is generic worker-thread work | state that document/style/image/script/cache/timer messages carry `(TabId,generation)`, while bookmark/session load/save/ack are the two browser-global message families; bookmark and session workers are the explicit private-disk exceptions and are joined on shutdown | `Msg::page`; wrong-tab tests; persistence worker tests |
| `CLAUDE.md` pipeline/scroll wording | navigation always runs everything; scroll merely “repaints” | replace with “fresh/cached/restored representations enter their appropriate boundary; redirects continue one generation; scroll re-renders cached display-list geometry with fixed/sticky viewport adjustment and no style/layout; live fixed-size controls may repaint without layout” | exact stage counters and closeout measurements |
| `CLAUDE.md` performance/DOD | one-page budgets and before/after only | preserve the 100 MB one-page budget; add that milestone closeouts report integrated multi-tab/persistence memory separately, use warm-up-discarded alternating A/B samples where work is comparable, and separate worker/disk/shutdown latency from UI input latency | final `perf.md` M11 section |

## Remaining gaps

There are no unproven M11 acceptance claims or unexplained ladder/golden
movements. The deliberate limitations in `DEVIATIONS.md` remain product scope,
not closeout failures. PLAN/CLAUDE are intentionally stale until the human
applies the exact proposal above, as required by this task; temporary captures,
worktrees, loopback ports and isolated profile files are not repository
artifacts.
