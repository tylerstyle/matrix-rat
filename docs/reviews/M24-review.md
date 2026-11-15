# M24 Review — Room Threads List (`threads-list`)

Reviewed `main...threads-list` (`9e7bd00..161ddd4`).

## Verdict
**Approved** — clean, well-bounded, and ready to merge.

## Checklist Verification

1. **Stale answers (`room_id` + `list_gen`):**
   - `App::next_threads_generation` (`src/app/thread_list.rs:52-55`) increments `self.threads.list_gen` on `open_threads` and `toggle_threads_mine`.
   - `App::threads_popup_for` (`src/app/thread_list.rs:264-275`) requires `popup.room_id == room_id && popup.generation == generation && popup.load == ThreadsLoad::Loading`, dropping answers from earlier generations, other rooms, or after the modal closed.
   - `Modal::Threads(_)` is cleared on `reset_session` (`src/app.rs:1255`) and `Esc` (`src/app.rs:2728`).

2. **Paging bounds and token-repeat stop:**
   - **Worker (`src/matrix/threads.rs:248-327`):** `LIST_PAGE = 25` (enforced both in `ListThreadsOptions::limit` and `.take(LIST_PAGE as usize)` on `roots.chunk`), `LIST_TIMEOUT = 20s`, `LIST_TEXT = 200`, `LIST_NAME = 64`, and `LIST_TOKEN = 1024` (`prev_batch_token.filter(|token| !token.is_empty() && token.len() <= LIST_TOKEN)`).
   - **UI (`src/modal.rs:843-939`):** `ThreadsPopup::MAX_THREADS = 500`, `FILTER_MAX_BYTES = 256`, deduplication by `known.root == thread.root`, and token-repeat stop `let again = next.is_some() && next == self.asked;` transitions `load` to `ThreadsLoad::Done`. Receiving a page in `on_threads` never auto-chains another request without user scrolling/navigation.

3. **Opening a thread whose root is not loaded (`jump_to_event`):**
   - `App::open_listed_thread` (`src/app/thread_list.rs:280-307`) checks `self.room_messages_of(index)` when `index == self.active` (preserving `view.room_messages` when another thread is currently open), calling `self.leave_thread()` + `self.open_thread(at)` when loaded, and falling back to `self.jump_to_event(room_id, root.clone(), Some(root))` when the root is further up or in another room.

4. **Ignored users:**
   - `threads::describe` (`src/matrix/threads.rs:340-362`) drops threads whose root sender is in `ignored` (`sender != own && ignored.contains(&sender)`) and suppresses `latest_when`, `unread`, and `latest_sender` when `bundled_latest_thread_event()` is from an ignored user.

5. **Privacy of `Debug` and logs:**
   - Custom `Debug` implementations on `MatrixCmd::ListThreads`, `MatrixEvent::Threads`, `MatrixEvent::ThreadsFailed`, `ThreadPage`, `ThreadsPopup`, and `ThreadInfo` log only generation numbers, counts, and booleans—never room IDs, event IDs, continuation tokens, sender names, or message text.
   - `threads::list` (`src/matrix/threads.rs:304-314`) logs only the HTTP status code (`?status`) on failure.

6. **Worker task lifetime:**
   - `Timelines::thread_lister` (`src/matrix/rooms.rs:106, 180-185, 1195-1230`) aborts any previous in-flight request when a new `ListThreads` arrives and is aborted in `Timelines::stop()` at session end while remaining independent of `Timelines::close()` so listing threads from the sidebar for a non-active room is not interrupted.
