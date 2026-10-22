# Graph Report - beacon  (2026-10-22)

## Corpus Check
- 63 files · ~160,425 words
- Verdict: corpus is large enough that graph structure adds value.
- Unclassified: 6 file(s) not represented in the graph (top: (none) 3, .css 2, .example 1)

## Summary
- 2588 nodes · 7717 edges · 101 communities (91 shown, 10 thin omitted)
- Extraction: 94% EXTRACTED · 6% INFERRED · 0% AMBIGUOUS · INFERRED: 476 edges (avg confidence: 0.86)
- Token cost: 289,318 input · 0 output

## Community Hubs (Navigation)
- TUI Render Tests
- Sync Integration Tests
- Wrapper Script Tests
- Store Tests
- SSE Stream And Long Poll
- HTTP Routes And Uploads
- CLI Output Tests
- Server State And Extractors
- Linear Design Tokens
- CLI Client And SSE Tests
- CLI Human Formatting
- TUI Key And Input Limits
- TUI Theme And Palette
- TUI Terminal Lifecycle
- Store SQL Mapping
- TUI App State
- TUI Key Handling
- CLI Process And Retry Tests
- Store Ingest And Collisions
- Post Rendering And Sanitizing
- Unread And Cursors
- Server Auth Tests
- Server Channel And Unread Tests
- CLI Entry And Parsing
- Text Width And Wrapping
- Model Validation And Limits
- Sync Push And Version Vectors
- Sync Peer Loops
- TUI Live Pty Tests
- TUI Dialogs And Layout
- HTTP Client Core
- CLI Channel And Attach Commands
- Exact-Size Stdin Reader
- CLI Command Tree
- TUI Worker And Feed
- Blob Store Tests
- Client Read Endpoints
- Userspace Tailscale Compose
- Model Tests
- Sync Peer Identity
- Sync HTTP Headers
- Upload Limit Tests
- Agent Skill Protocol
- Sync Loss Detection
- Handoff Decisions
- AppState Write Paths
- Attach And Get Tests
- TUI Live Flow Tests
- API Wire Types
- Peer HTTP Client
- TUI Run And Panic Hook
- Unread And Presence Decisions
- Board Concepts Overview
- Event Model Types
- Client Streaming And Files
- Mockup Fixture Extractor
- Event Paging Tests
- TUI Sidebar
- Blob Store
- TUI Canvas Primitives
- Agent Coordination Notes
- Exchange And Announcements Docs
- Sync Design And Open Questions
- Stream Event Types
- Error And Exit Codes
- Cursor And Join Tests
- Hashing And Async IO
- CLI Reference Docs
- API To View Conversion
- Long Poll And Push Tests
- CLI Get Command
- Workflow And Quality Bar
- Configuration
- TUI View Layout
- CLI Reconnect Backoff
- Machines And Repo Setup
- Client Post Queries
- TUI Composer
- API Extras And Wait Decisions
- SOCKS Proxy Connector
- JSON Schema Docs
- Sync Backoff
- Test HTTP Helpers
- Sync Peer Table
- Event Log Design
- Blob Errors
- Sync Blob Fetch Jobs
- CLI 5xx Proxy Tests
- SSE Recovery Tests
- Post And Who Docs
- Storage And Delete Docs
- Peer Request Counter
- Blob Download Stream
- Handle Validation
- Watch Output
- TUI Send Failure States
- Binary Entry Point
- Join And Leave
- Cargo Manifest

## God Nodes (most connected - your core abstractions)
1. `spawn_node()` - 161 edges
2. `App` - 102 edges
3. `beacon()` - 74 edges
4. `AppState` - 69 edges
5. `Store` - 65 edges
6. `TestNode` - 65 edges
7. `ok()` - 61 edges
8. `Cmd` - 57 edges
9. `Event` - 54 edges
10. `Client` - 53 edges

## Surprising Connections (you probably didn't know these)
- `peer_urls_are_normalized()` --calls--> `normalize_peer_url()`  [INFERRED]
  tests/sync.rs → src/sync.rs
- `queued_counts_what_a_peer_lacks_per_origin()` --calls--> `vv()`  [INFERRED]
  tests/sync.rs → src/sync.rs
- `Workflow Dang asked for` --semantically_similar_to--> `Requested skills: superpowers, impeccable, web-design-guidelines, graphify`  [INFERRED] [semantically similar]
  CLAUDE.md → docs/superpowers/specs/2026-10-21-beacon-design.md
- `JSON shapes table` --conceptually_related_to--> `JSON schema v1`  [INFERRED]
  skills/beacon/SKILL.md → README.md
- `Timer fallback to read --unread polling` --semantically_similar_to--> `Timer-based retry fallback`  [INFERRED] [semantically similar]
  skills/beacon/SKILL.md → README.md

## Import Cycles
- 1-file cycle: `src/tui/widgets/header.rs -> src/tui/widgets/header.rs`
- 1-file cycle: `src/tui/theme.rs -> src/tui/theme.rs`
- 2-file cycle: `src/config.rs -> src/sync/http.rs -> src/config.rs`
- 2-file cycle: `src/store/mod.rs -> src/store/unread.rs -> src/store/mod.rs`
- 2-file cycle: `src/tui/convert.rs -> src/tui/state.rs -> src/tui/convert.rs`
- 2-file cycle: `src/tui/state.rs -> src/tui/widgets/posts.rs -> src/tui/state.rs`
- 2-file cycle: `src/tui/view.rs -> src/tui/widgets/posts.rs -> src/tui/view.rs`
- 2-file cycle: `src/config.rs -> src/sync.rs -> src/config.rs`
- 2-file cycle: `src/blobs.rs -> src/server/stream.rs -> src/blobs.rs`
- 2-file cycle: `src/tui/state.rs -> src/tui/view.rs -> src/tui/state.rs`
- 2-file cycle: `src/tui/view.rs -> src/tui/widgets/title.rs -> src/tui/view.rs`
- 2-file cycle: `src/tui/view.rs -> src/tui/widgets/helpbar.rs -> src/tui/view.rs`
- 2-file cycle: `src/tui/view.rs -> src/tui/widgets/composer.rs -> src/tui/view.rs`
- 2-file cycle: `src/model.rs -> src/sync.rs -> src/model.rs`
- 2-file cycle: `src/server/mod.rs -> src/sync.rs -> src/server/mod.rs`
- 3-file cycle: `src/blobs.rs -> src/server/stream.rs -> src/server/routes.rs -> src/blobs.rs`
- 3-file cycle: `src/config.rs -> src/sync.rs -> src/sync/http.rs -> src/config.rs`
- 3-file cycle: `src/model.rs -> src/sync.rs -> src/sync/http.rs -> src/model.rs`
- 3-file cycle: `src/model.rs -> src/sync.rs -> src/store/mod.rs -> src/model.rs`
- 3-file cycle: `src/api.rs -> src/store/mod.rs -> src/tui/state.rs -> src/api.rs`

## Hyperedges (group relationships)
- **Idempotent event-log sync mechanism** — docs_superpowers_specs_2026_10_21_beacon_design_event_log, docs_superpowers_specs_2026_10_21_beacon_design_n_device_sync, docs_superpowers_specs_2026_10_21_beacon_design_push_first_timers_as_fallback, docs_superpowers_specs_2026_10_21_beacon_design_peer_discovery, docs_superpowers_specs_2026_10_21_beacon_design_content_addressed_attachments [INFERRED 0.85]
- **Agent wake-up and unread flow** — docs_superpowers_specs_2026_10_21_beacon_design_waking_agents_wait_and_watch, docs_superpowers_specs_2026_10_21_beacon_design_unread_cursors_and_joins, docs_superpowers_specs_2026_10_21_beacon_design_announcements, docs_superpowers_specs_2026_10_21_beacon_design_timed_read_fallback [INFERRED 0.85]
- **Plan review focus areas** — docs_superpowers_plans_2026_10_21_beacon_review_focus_late_arriving_post, docs_superpowers_plans_2026_10_21_beacon_review_focus_verbatim_message_text, docs_superpowers_plans_2026_10_21_beacon_review_focus_wide_characters, docs_superpowers_plans_2026_10_21_beacon_review_focus_server_down, docs_superpowers_plans_2026_10_21_beacon_review_focus_peer_list_dedupe [EXTRACTED 1.00]
- **Userspace Tailscale networking configuration** — compose_userspace_host_network, compose_userspace_ports_reset, compose_userspace_env_overrides, compose_userspace_socks5_proxy, readme_userspace_tailscale_setup [EXTRACTED 1.00]
- **Agent wake-up and catch-up loop (wait, then read --unread, with timer fallback)** — skills_beacon_skill_cmd_wait_watch, skills_beacon_skill_wait_never_marks_read, skills_beacon_skill_cmd_read, skills_beacon_skill_timer_fallback, readme_cmd_wait, readme_cmd_watch [EXTRACTED 1.00]
- **Peer-to-peer sync strategy** — readme_push_first_sync, readme_timer_fallback_sync, readme_peer_discovery, readme_attachment_sync, readme_channel_convergence, readme_event_log [EXTRACTED 1.00]

## Communities (101 total, 10 thin omitted)

### Community 0 - "TUI Render Tests"
Cohesion: 0.05
Nodes (127): ChannelJson, ChannelsJson, PostJson, Loaded, Agents, Channels, Files, Posts (+119 more)

### Community 1 - "Sync Integration Tests"
Cohesion: 0.05
Nodes (85): PushRequest, Query, Config, reserve(), Reserved, spawn_node_with(), StoppedNode, test_config() (+77 more)

### Community 2 - "Wrapper Script Tests"
Cohesion: 0.08
Nodes (57): a_signal_while_the_stdin_is_being_made_removes_its_directory(), attach_missing_file_exits_2_without_calling_docker(), attach_relative_path_from_other_dir(), attach_size_survives_a_wc_that_pads_its_answer(), attach_size_takes_a_value_and_is_for_stdin_only(), attach_streams_host_file(), attach_tells_the_cli_the_files_size(), attach_with_stdin_dash_is_left_to_the_cli() (+49 more)

### Community 3 - "Store Tests"
Cohesion: 0.10
Nodes (64): a_collision_on_another_nodes_origin_keeps_what_is_stored(), a_new_derive_version_rebuilds_the_board_from_the_log(), a_rolled_back_tail_replayed_from_a_peer_ends_with_both_posts_everywhere(), an_unknown_kind_is_stored_and_relayed_never_derived(), announced_peers_latest_per_node(), apply_all_keeps_the_rules_of_each_event(), apply_all_stores_a_page_in_one_transaction(), apply_is_atomic() (+56 more)

### Community 4 - "SSE Stream And Long Poll"
Cohesion: 0.05
Nodes (32): parse_vv(), CATCH_UP_PAGE, describe(), EVENTS_PAGE, events_wait(), EVENTS_WAIT_MAX, Feed, INSTANCE_HEADER (+24 more)

### Community 5 - "HTTP Routes And Uploads"
Cohesion: 0.09
Nodes (44): HealthJson, JoinsRequest, PostCreated, health_text(), ApiJson, ApiPath, ApiQuery, attachment_name() (+36 more)

### Community 6 - "CLI Output Tests"
Cohesion: 0.12
Nodes (49): a_fresh_post_has_no_day_line(), a_good_download_replaces_the_target_without_a_trace(), an_ambiguous_id_suffix_exits_5_listing_candidates(), api_posts(), attach_names_with_quotes_and_unicode_survive(), attach_says_what_it_posted(), attach_usage_errors(), beacon() (+41 more)

### Community 7 - "Server State And Extractors"
Cohesion: 0.08
Nodes (19): ApiFailure, ApiJson<T>, ApiPath<T>, ApiQuery<T>, ARRIVALS_CAPACITY, io_failure(), method_not_allowed(), no_such_endpoint() (+11 more)

### Community 8 - "Linear Design Tokens"
Cohesion: 0.08
Nodes (48): Acid Lime #e4f222, Agent Prompt Guide, Ash #62666d, Badge / Status Tag, Berkeley Mono typeface, Bone #e5e5e6, Border radius tokens, Carbon #0f1011 (+40 more)

### Community 9 - "CLI Client And SSE Tests"
Cohesion: 0.07
Nodes (36): a_post_header_follows_the_mockup(), a_refused_port_is_refused(), a_stream_the_node_closes_ends_with_an_interrupted_error(), an_attachment_line_names_the_command_that_fetches_it(), assert_clock_time(), assert_graphemes_whole(), body_text_is_wrapped_at_100_columns_and_cleaned(), channel_event() (+28 more)

### Community 10 - "CLI Human Formatting"
Cohesion: 0.07
Nodes (37): ChannelAck, PresenceJson, WhoJson, ago(), break_word(), channel_ack_text(), channels_text(), clean_body() (+29 more)

### Community 11 - "TUI Key And Input Limits"
Cohesion: 0.06
Nodes (43): BODY_MAX, TOPIC_MAX, BODY_FULL, check_name(), form_char(), form_key(), FormOutcome, Close (+35 more)

### Community 12 - "TUI Theme And Palette"
Cohesion: 0.07
Nodes (36): ACCENT, ACTIVE, ASH, CARBON, ColorMode, Ansi256, TrueColor, CORAL (+28 more)

### Community 13 - "TUI Terminal Lifecycle"
Cohesion: 0.05
Nodes (20): hex(), ACTIVE, BATCH, CLIENT_NAME, CONTAINER_EXCHANGE, create_hidden(), create_unique(), DEFAULT_HUMAN (+12 more)

### Community 14 - "Store SQL Mapping"
Cohesion: 0.06
Nodes (23): AuthorKind, channel_row(), CHANNEL_SELECT, ChannelRow, derive(), DERIVE_VERSION, FORMER_DB_ID, gapless_from() (+15 more)

### Community 15 - "TUI App State"
Cohesion: 0.10
Nodes (17): post_extra(), App, Arrivals, AttachmentItem, before_cursor(), Conn, Down, Up (+9 more)

### Community 16 - "TUI Key Handling"
Cohesion: 0.09
Nodes (20): App, printable(), Cmd, CreateChannel, DeleteChannel, DeletePost, Heartbeat, ListFiles (+12 more)

### Community 17 - "CLI Process And Retry Tests"
Cohesion: 0.14
Nodes (28): a_dropped_request_is_not_a_missing_node(), a_post_that_dies_in_flight_is_not_retried(), attach_exits_when_it_has_failed_even_if_stdin_stays_open(), Child, free_port(), join_channel(), join_then_wait_wakes(), listeners() (+20 more)

### Community 18 - "Store Ingest And Collisions"
Cohesion: 0.09
Nodes (17): Event, Ingested, may_add_unread(), Applied, Batch, parse_event(), PostQuery, Saved (+9 more)

### Community 19 - "Post Rendering And Sanitizing"
Cohesion: 0.10
Nodes (32): mentions_handle(), is_unsafe(), active_filter(), BADGE_MARGIN, BODY_MARGIN, body_segs(), clean(), day_separator() (+24 more)

### Community 20 - "Unread And Cursors"
Cohesion: 0.07
Nodes (17): committed_post(), json_column(), POST_COLUMNS, post_row(), PostRow, announcements_sql(), count_unread_sql(), counts_sql() (+9 more)

### Community 21 - "Server Auth Tests"
Cohesion: 0.08
Nodes (23): agent_seen_events(), bad_input_is_400_usage(), commit_if_stamps_the_event_after_taking_the_lock(), config_requires_node_and_secret(), config_validates_names_and_numbers_naming_the_variable(), event_post_id(), events_bad_queries_are_400(), every_route_requires_the_secret() (+15 more)

### Community 22 - "Server Channel And Unread Tests"
Cohesion: 0.11
Nodes (37): agent_post(), arrivals_broadcast_only_what_derived(), channel_bodies(), channels_carry_unread_when_a_handle_is_given(), channels_count_only_relevant_posts_with_relevant_1(), create_channel_normalizes_and_is_idempotent(), create_post_read_back(), cross_channel_queries() (+29 more)

### Community 23 - "CLI Entry And Parsing"
Cohesion: 0.07
Nodes (26): CLIENT_WENT_AWAY, DEFAULT_HUMAN, EXIT_ON_STDIN_EOF, HEARTBEAT_EVERY, init_tracing(), is_broken_pipe(), open_attachment(), print_text() (+18 more)

### Community 24 - "Text Width And Wrapping"
Cohesion: 0.13
Nodes (20): possessive(), render(), status_variants(), FILTER_HINT, GAP, KEYS, PAD, render() (+12 more)

### Community 25 - "Model Validation And Limits"
Cohesion: 0.10
Nodes (32): NewChannelRequest, WhoAck, WhoRequest, ATTACHMENT_NAME_MAX, CHANNEL_MAX, check_channel(), check_post(), check_text() (+24 more)

### Community 26 - "Sync Push And Version Vectors"
Cohesion: 0.07
Nodes (24): PushResponse, format_vv(), Shutdown, BATCH, BLOB_SWEEP, IDLE_PULL_GAP, INSTANCE_HEADER, MAX_BACKOFF (+16 more)

### Community 27 - "Sync Peer Loops"
Cohesion: 0.15
Nodes (4): lock(), pull_loop(), push_loop(), SyncHandle

### Community 28 - "TUI Live Pty Tests"
Cohesion: 0.08
Nodes (9): SECRET, contains(), cooked(), DEADLINE, open_pty(), screen_text(), sighup_restores_the_terminal(), signal_restores_the_terminal() (+1 more)

### Community 29 - "TUI Dialogs And Layout"
Cohesion: 0.20
Nodes (17): HELP, frame(), Layout, attach(), BUTTONS_INSET, confirmation(), help(), HELP_KEYS (+9 more)

### Community 30 - "HTTP Client Core"
Cohesion: 0.09
Nodes (13): CONNECT_TIMEOUT, DEFAULT_SERVER, Download, MAYBE_STORED, OpenStream, REQUEST_TIMEOUT, SSE_LINE_MAX, SseMessage (+5 more)

### Community 31 - "CLI Channel And Attach Commands"
Cohesion: 0.22
Nodes (20): attach(), AttachArgs, channel_delete(), channel_new(), channels(), check_before_reposting(), Ctx, delete() (+12 more)

### Community 32 - "Exact-Size Stdin Reader"
Cohesion: 0.13
Nodes (9): drain(), Exact, Exact<R>, Mismatch, Early, Long, short_and_long_streams_fail_with_the_count(), the_right_length_passes_untouched() (+1 more)

### Community 33 - "CLI Command Tree"
Cohesion: 0.07
Nodes (27): ChannelAction, Delete, New, Cli, Command, Attach, Channel, Channels (+19 more)

### Community 34 - "TUI Worker And Feed"
Cohesion: 0.11
Nodes (17): input_msg(), live_feed(), ticks(), Worker, Msg, ChannelDeleted, Created, Key (+9 more)

### Community 35 - "Blob Store Tests"
Cohesion: 0.19
Nodes (18): a_peers_attachment_is_unavailable_until_its_bytes_arrive(), chunked(), clear_temp_removes_leftovers_and_keeps_blobs(), dropping_an_upload_midway_removes_its_temp_file(), names(), names_that_are_not_a_sha_never_reach_the_disk(), open_gives_the_file_and_its_length(), put_stream_refuses_more_than_the_limit_and_cleans_up() (+10 more)

### Community 36 - "Client Read Endpoints"
Cohesion: 0.19
Nodes (4): CursorsJson, DeleteAck, JoinsJson, Client

### Community 37 - "Userspace Tailscale Compose"
Cohesion: 0.15
Nodes (24): Environment overrides (BEACON_LISTEN, BEACON_SERVER, BEACON_PROXY), network_mode host, compose.userspace.yml override, ports reset with !reset, tailscaled SOCKS5 proxy socks5h://127.0.0.1:1055, beacon compose service, env_file .env, Port publish BEACON_BIND:BEACON_PORT to 7700 (+16 more)

### Community 38 - "Model Tests"
Cohesion: 0.09
Nodes (3): day_labels(), validate_event_checks_what_peers_name(), validate_event_enforces_free_text_limits()

### Community 39 - "Sync Peer Identity"
Cohesion: 0.15
Nodes (13): a_new_announcement_replaces_the_old_one(), a_url_answering_as_another_node_forgets_what_the_old_one_had(), a_url_that_answers_as_us_is_dropped_for_good(), configured_urls_are_normalized_deduped_and_never_ourselves(), Learned, Gone, Other, Retry (+5 more)

### Community 40 - "Sync HTTP Headers"
Cohesion: 0.11
Nodes (8): CONNECT_TIMEOUT, endpoint(), endpoints_spell_out_the_port(), ERROR_TEXT_MAX, JSON_MAX, parse_proxy(), proxies_are_socks5(), STALL

### Community 41 - "Upload Limit Tests"
Cohesion: 0.18
Nodes (20): attachment_names_are_reduced_to_a_file_name(), attachment_over_limit_413(), bad_uploads_are_refused_before_a_file_is_stored(), file_part(), files_in(), free_text_limits_are_400_at_the_edge(), identical_uploads_share_one_blob(), json_posts_keep_their_2_mib_limit_and_the_upload_limit_is_per_node() (+12 more)

### Community 42 - "Agent Skill Protocol"
Cohesion: 0.15
Nodes (20): channels command, Claude Code and Codex agents, Dang's announcements outrank other requests, Beacon agent skill (SKILL.md), Claim-before-you-start protocol, channels and channel usage guidance, join, leave and who usage guidance, read usage guidance (+12 more)

### Community 43 - "Sync Loss Detection"
Cohesion: 0.21
Nodes (15): a_pull_answer_raises_the_view_to_it_and_no_further(), a_pull_answer_with_less_than_it_was_asked_with_is_believed_once(), a_push_answer_with_less_than_the_batch_was_chosen_from_is_believed(), a_url_that_changes_names_again_soon_does_not_start_over_again(), behind(), covers(), known(), merge_vv() (+7 more)

### Community 44 - "Handoff Decisions"
Cohesion: 0.13
Nodes (13): Project overview and doc map, Approved look changes (metadata grey, coral toasts, named delete dialog), Limit: TUI survives terminal death inside the container, Mockup fixtures extracted from tui-mockup.html, Short post ids (last 8 ULID chars), Task 10: TUI rendering matching the mockup, TUI palette and role table, Announcements (Dang's posts) (+5 more)

### Community 45 - "AppState Write Paths"
Cohesion: 0.18
Nodes (14): AppState, now_ms(), run_on_store(), who(), announce_self(), download(), fetch_blob(), fetch_blobs() (+6 more)

### Community 46 - "Attach And Get Tests"
Cohesion: 0.23
Nodes (18): a_download_that_does_not_match_its_post_leaves_the_target_alone(), assert_no_blobs(), attach_and_get_roundtrip(), attach_over_the_limit_is_refused_in_the_nodes_words(), attach_size_goes_with_stdin_and_is_a_number(), attach_sized(), attach_with_size_posts_a_file_of_exactly_that_size(), attach_with_size_refuses_a_file_that_ended_early() (+10 more)

### Community 47 - "TUI Live Flow Tests"
Cohesion: 0.27
Nodes (12): a_wrong_secret_shows_the_rejected_band(), attach_save_and_delete_through_the_keys(), general_unread(), get_json(), new_channel_from_the_keys(), post_as(), posts_as_dang_and_marks_read(), save_refuses_a_file_that_does_not_match_its_post() (+4 more)

### Community 48 - "API Wire Types"
Cohesion: 0.11
Nodes (13): ACTIVE_WINDOW_MS, ApiError, CursorRequest, JoinAck, PeersJson, StatusJson, V, peers_list_text() (+5 more)

### Community 49 - "Peer HTTP Client"
Cohesion: 0.20
Nodes (5): lock(), PeerClient, read_json(), success(), within()

### Community 50 - "TUI Run And Panic Hook"
Cohesion: 0.13
Nodes (9): drive(), install_panic_hook(), restore_terminal(), run(), run_on_terminal(), stop_signal(), TuiConfig, coalesce() (+1 more)

### Community 51 - "Unread And Presence Decisions"
Cohesion: 0.14
Nodes (12): Limit: presence heartbeats are permanent replicated events, Next: log in the claude CLI on the PC, Beacon skill installed for Claude Code, Task 13: skill file and README, Task 1: crate scaffold, model, time formatting, Task 2: Store event log and derived state, Task 3: unread, relevance, cursors, joins, Beacon skill file (SKILL.md) (+4 more)

### Community 52 - "Board Concepts Overview"
Cohesion: 0.14
Nodes (16): Attachment sync, Beacon message board, beacon Rust binary (server, CLI, TUI), BEACON_MAX_ATTACHMENT_MB, Channels, attach command, channel new and channel delete commands, get command (+8 more)

### Community 53 - "Event Model Types"
Cohesion: 0.12
Nodes (15): Attachment, AuthorKind, Agent, Human, EventKind, AgentSeen, ChannelCreated, ChannelDeleted (+7 more)

### Community 55 - "Mockup Fixture Extractor"
Cohesion: 0.21
Nodes (8): apply_sgr(), fail(), load_screens(), main(), parse_screen(), row_runs(), row_text(), write_screen()

### Community 56 - "Event Paging Tests"
Cohesion: 0.17
Nodes (14): EventsResponse, a_cut_page_is_followed_with_wait_zero_while_pages_advance(), applied(), events_limit_marks_a_cut_page_as_more(), events_page(), events_page_has_the_agreed_shape_and_names_the_node(), events_path(), long_poll_wakes_on_new_event() (+6 more)

### Community 58 - "TUI Sidebar"
Cohesion: 0.23
Nodes (11): age_short(), sync_age(), announcements(), draw(), GLYPH, label(), NAME, panel() (+3 more)

### Community 59 - "Blob Store"
Cohesion: 0.27
Nodes (4): Blobs, io_error(), is_sha256(), Received

### Community 61 - "Agent Coordination Notes"
Cohesion: 0.21
Nodes (11): Beacon CLAUDE.md project instructions, Agent message prefixes [PC agent] and [Mac agent], Slack #bots coordination until beacon works, Handoff 2026-10-22, Next: switch agent coordination from Slack to beacon, Beacon implementation plan, Task 12: Docker, compose, wrapper packaging, Beacon design spec (+3 more)

### Community 62 - "Exchange And Announcements Docs"
Cohesion: 0.20
Nodes (11): ./exchange to /exchange bind mount, Announcements (Dang's posts), BEACON_DATA and BEACON_EXCHANGE, BEACON_HUMAN, status, peers and health commands, ./exchange folder, Duplicate BEACON_NODE name clash, Terminal UI (TUI) (+3 more)

### Community 63 - "Sync Design And Open Questions"
Cohesion: 0.16
Nodes (11): Next: cross-machine end-to-end test, Open questions: MagicDNS in Docker Desktop, Ctrl-C on watch, first post after wake, Blobs (attachments by sha256), Sync engine (puller, pusher, backoff, discovery), Task 5: attachments, Task 7: sync engine, Content-addressed attachments, N-device sync over Tailscale (+3 more)

### Community 64 - "Stream Event Types"
Cohesion: 0.18
Nodes (10): samples(), stream_event_data_has_the_documented_shapes(), stream_events_round_trip_through_their_server_sent_form(), StreamEvent, Channel, Peers, Post, PostDeleted (+2 more)

### Community 65 - "Error And Exit Codes"
Cohesion: 0.15
Nodes (10): Error, Ambiguous, Auth, Conflict, Internal, Interrupted, NotFound, Timeout (+2 more)

### Community 66 - "Cursor And Join Tests"
Cohesion: 0.29
Nodes (14): agent_post(), cursor_never_moves_backwards(), cursors_and_joins_are_local_only(), cursors_and_joins_persist_across_reopen(), dang_post(), mark_read_advances_and_limits(), reading_one_channel_leaves_the_others_unread(), relevance_ignores_handle_case() (+6 more)

### Community 67 - "Hashing And Async IO"
Cohesion: 0.15
Nodes (4): MIB, NOT_A_BLOB, TEMP_PREFIX, TempFile

### Community 68 - "CLI Reference Docs"
Cohesion: 0.22
Nodes (12): BEACON_AGENT and BEACON_CLIENT, BEACON_EXIT_ON_STDIN_EOF, CLI reference, join and leave commands, read command, skill show and skill install commands, wait command, watch command (+4 more)

### Community 69 - "API To View Conversion"
Cohesion: 0.27
Nodes (11): PeerJson, agent_item(), channel_item(), clean_line(), last_sync_ms(), peer_item(), post_item(), AgentItem (+3 more)

### Community 70 - "Long Poll And Push Tests"
Cohesion: 0.23
Nodes (12): long_poll_answers_at_once_when_the_page_holds_something_new(), long_poll_with_a_head_from_another_database_answers_at_once(), push_takes_a_batch_with_an_unknown_kind(), pushed_posts_reach_streams_and_waiters_within_a_second(), remote_post(), start_wait(), wait_blocks_then_wakes_on_mention(), wait_ignores_own_and_unjoined() (+4 more)

### Community 71 - "CLI Get Command"
Cohesion: 0.26
Nodes (10): AttachmentJson, destination(), get(), GetArgs, io_error(), matches_post(), mismatch(), not_synced() (+2 more)

### Community 72 - "Workflow And Quality Bar"
Cohesion: 0.20
Nodes (11): Graphify knowledge graph guidance, Quality bar: TDD, cargo test, clippy, fmt, shellcheck, Upstream skill sources, Workflow Dang asked for, Knowledge graph in graphify-out, Client (async HTTP for CLI and TUI), Task 11: TUI behaviour, keys, live data, dialogs, Task 14: TUI craft review and polish (+3 more)

### Community 73 - "Configuration"
Cohesion: 0.17
Nodes (7): DEFAULT_DATA, DEFAULT_HUMAN, DEFAULT_LISTEN, DEFAULT_MAX_ATTACHMENT_MB, DEFAULT_SSE_KEEP_ALIVE, BASE_BACKOFF, LONG_POLL_SECS

### Community 74 - "TUI View Layout"
Cohesion: 0.23
Nodes (8): MIN_HEIGHT, MIN_WIDTH, render(), ROWS_ABOVE_POSTS, ROWS_BELOW_COMPOSER, SIDEBAR_NARROW, SIDEBAR_WIDE, too_small()

### Community 75 - "CLI Reconnect Backoff"
Cohesion: 0.32
Nodes (9): Backoff, heartbeat_every(), say_reconnecting(), secs(), wait(), wait_pauses_are_shorter(), WaitArgs, watch() (+1 more)

### Community 76 - "Machines And Repo Setup"
Cohesion: 0.27
Nodes (11): Private GitHub repo FluxxDB/beacon (not yet created), Mac machine (Docker Desktop, kernel Tailscale), PC machine (WSL2, userspace Tailscale, no sudo), Next: create private FluxxDB/beacon GitHub repo, Next: Mac side setup, PC tailscaled SOCKS5 proxy enabled and verified, Beacon stack running on the PC, Beacon v1 built, reviewed and merged (+3 more)

### Community 78 - "TUI Composer"
Cohesion: 0.24
Nodes (9): HINT, HINT_WIDE, input_lines(), line_count(), MAX_LINES, placeholder(), PROMPT, render() (+1 more)

### Community 79 - "API Extras And Wait Decisions"
Cohesion: 0.33
Nodes (9): Extra endpoints: /api/unread and /api/wait, Plan global constraints, Task 4: HTTP server, API types, auth, Task 6: SSE stream, event long-poll, push intake, wait, Task 9: CLI part 2 (attach, get, who, join, wait, watch, skill), CLI agent contract, HTTP JSON API, Security: tailnet-only bind and bearer secret (+1 more)

### Community 80 - "SOCKS Proxy Connector"
Cohesion: 0.24
Nodes (3): Pool, Direct, Socks

### Community 81 - "JSON Schema Docs"
Cohesion: 0.38
Nodes (10): ApiError, AttachmentJson, ChannelJson, HTTP API, JSON schema v1, PeerJson, PostJson, PresenceJson (+2 more)

### Community 82 - "Sync Backoff"
Cohesion: 0.27
Nodes (4): a_wait_is_cut_short_only_after_the_base(), Backoff, backoff_doubles_from_the_base_up_to_two_minutes(), failures_during_a_wait_count_once()

### Community 83 - "Test HTTP Helpers"
Cohesion: 0.29
Nodes (3): post_to_missing_channel_404_unless_create(), read_cursor_beyond_the_arrival_head_is_400(), read_cursor_for_an_unknown_channel_is_404()

### Community 84 - "Sync Peer Table"
Cohesion: 0.33
Nodes (3): Delivery, Candidate, Peer

### Community 85 - "Event Log Design"
Cohesion: 0.25
Nodes (4): Limit: crash loop can still split the board, Upgrade every node together, Plan architecture: immutable events, Store, sync engine, HTTP clients, Store (rusqlite event log)

### Community 86 - "Blob Errors"
Cohesion: 0.28
Nodes (4): Error, PutError, Failed, TooLarge

### Community 87 - "Sync Blob Fetch Jobs"
Cohesion: 0.25
Nodes (6): BlobJob, RetryAll, Sweep, Want, Inner, Receivers

### Community 89 - "SSE Recovery Tests"
Cohesion: 0.33
Nodes (4): sse_recovers_from_the_store_after_lagging(), sse_sends_a_keep_alive_comment_when_silent(), sse_streams_each_kind_of_event(), SseReader

### Community 90 - "Post And Who Docs"
Cohesion: 0.25
Nodes (6): post command, who command, CLI exit codes (0 to 6), Tags, @mentions and presence, post usage guidance, Exit codes with agent actions

### Community 91 - "Storage And Delete Docs"
Cohesion: 0.33
Nodes (6): beacon-data volume at /data, delete command, Event log with per-node sequence numbers, Immutable posts and tombstones, SQLite store and attachment files, delete usage guidance

### Community 94 - "Handle Validation"
Cohesion: 0.50
Nodes (3): human_handle(), validate_handle(), put_cursor()

### Community 95 - "Watch Output"
Cohesion: 0.50
Nodes (3): print_line(), show_watched(), watch_once()

### Community 96 - "TUI Send Failure States"
Cohesion: 0.67
Nodes (3): SendFailed, MaybeSent, NotSent

### Community 98 - "Join And Leave"
Cohesion: 0.67
Nodes (3): Membership, Join, Leave

## Knowledge Gaps
- **139 isolated node(s):** `beacon`, `Post`, `PostDeleted`, `Channel`, `Presence` (+134 more)
  These have ≤1 connection - possible missing edges or undocumented components. (Counts symbols only; 530 node(s) total have ≤1 connection when file, concept and rationale nodes are included.)
- **10 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `Error` connect `Error And Exit Codes` to `TUI Render Tests`, `Store Tests`, `SSE Stream And Long Poll`, `HTTP Routes And Uploads`, `Server State And Extractors`, `CLI Client And SSE Tests`, `TUI Key And Input Limits`, `TUI Terminal Lifecycle`, `Store SQL Mapping`, `Server Auth Tests`, `CLI Entry And Parsing`, `Model Validation And Limits`, `Sync Push And Version Vectors`, `TUI Live Pty Tests`, `HTTP Client Core`, `Blob Store Tests`, `Model Tests`, `Sync HTTP Headers`, `Hashing And Async IO`, `Configuration`?**
  _High betweenness centrality (0.086) - this node is a cross-community bridge._
- **Are the 157 inferred relationships involving `spawn_node()` (e.g. with `a_peers_attachment_is_unavailable_until_its_bytes_arrive()` and `a_500_is_an_internal_error_the_client_can_recognize()`) actually correct?**
  _`spawn_node()` has 157 INFERRED edges - model-reasoned connections that need verification._
- **What connects `beacon`, `Post`, `PostDeleted` to the rest of the system?**
  _139 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `TUI Render Tests` be split into smaller, more focused modules?**
  _Cohesion score 0.05344827586206897 - nodes in this community are weakly interconnected._
- **Why does `Cmd` connect `TUI Key Handling` to `TUI Render Tests`, `TUI Worker And Feed`, `TUI Key And Input Limits`, `TUI Terminal Lifecycle`, `TUI App State`, `TUI Run And Panic Hook`?**
  _High betweenness centrality (0.043) - this node is a cross-community bridge._
- **Should `Sync Integration Tests` be split into smaller, more focused modules?**
  _Cohesion score 0.051618547681539804 - nodes in this community are weakly interconnected._
- **Why does `Client` connect `Client Read Endpoints` to `Sync Integration Tests`, `TUI Worker And Feed`, `API To View Conversion`, `Sync HTTP Headers`, `Client Post Queries`, `TUI Terminal Lifecycle`, `SOCKS Proxy Connector`, `TUI Run And Panic Hook`, `Client Streaming And Files`, `CLI Entry And Parsing`, `HTTP Client Core`, `CLI Channel And Attach Commands`?**
  _High betweenness centrality (0.035) - this node is a cross-community bridge._