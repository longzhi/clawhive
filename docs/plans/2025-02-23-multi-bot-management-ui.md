# Multi-Bot Management UI Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Enable users to add, remove, and monitor multiple bot connectors (Telegram/Discord) through the Web UI, saving configurations to YAML and providing status feedback.

**Architecture:** Extend the existing Axum backend with specific connector CRUD and status endpoints. The frontend will be enhanced with a more robust `/channels` page using shadcn/ui dialogs for adding connectors and real-time status indicators. Configuration changes will trigger a "Restart Required" banner since hot-reloading is out of scope.

**Tech Stack:** 
- Backend: Rust (Axum, Serde, tokio)
- Frontend: Next.js 16, React 19, TanStack Query 5, shadcn/ui
- Storage: YAML (`config/main.yaml`)

---

### Task 1: Backend - Add Connector Status Endpoint

**Files:**
- Modify: `crates/nanocrab-server/src/routes/channels.rs`
- Modify: `crates/nanocrab-server/src/main.rs` (if routing needs registration)

**Step 1: Write failing test**
Create a test that calls `GET /api/channels/status` and expects a 200 OK with a list of connector statuses.

```rust
#[tokio::test]
async fn test_get_channels_status() {
    let app = setup_test_app().await;
    let response = app.oneshot(Request::builder().uri("/api/channels/status").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}
```

**Step 2: Run test to verify it fails**
Run: `cargo test`
Expected: 404 Not Found

**Step 3: Implement endpoint**
Define `get_channels_status` in `channels.rs`. For now, it can return hardcoded statuses or check if tokens are set in `main.yaml`.

```rust
async fn get_channels_status(State(state): State<AppState>) -> Result<Json<Vec<ConnectorStatus>>, StatusCode> {
    // Implementation logic
}
```

**Step 4: Run test to verify it passes**
Run: `cargo test`
Expected: PASS

**Step 5: Commit**
```bash
git add crates/nanocrab-server/src/routes/channels.rs
git commit -m "feat: add channel status endpoint"
```

---

### Task 2: Backend - Add Connector CRUD Endpoints

**Files:**
- Modify: `crates/nanocrab-server/src/routes/channels.rs`

**API Design:**
- `POST /api/channels/:kind/connectors`: Add a new connector
- `DELETE /api/channels/:kind/connectors/:id`: Remove a connector

**Step 1: Write failing tests for POST and DELETE**
Tests should verify that `main.yaml` is updated correctly.

**Step 2: Run tests to verify they fail**
Run: `cargo test`
Expected: 404 or 405

**Step 3: Implement CRUD logic**
Update `update_channels` logic to support granular connector operations.

**Step 4: Run tests to verify they pass**
Run: `cargo test`
Expected: PASS

**Step 5: Commit**
```bash
git add crates/nanocrab-server/src/routes/channels.rs
git commit -m "feat: add connector CRUD endpoints"
```

---

### Task 3: Frontend - Update API Hooks and Types

**Files:**
- Modify: `web/src/hooks/use-api.ts`

**Step 1: Add new interfaces and hooks**
Add `ConnectorStatus` interface and `useChannelStatus`, `useAddConnector`, `useRemoveConnector` hooks.

**Step 2: Update existing `useChannels` types**
Ensure it matches the backend's `Vec<ConnectorConfig>` structure.

**Step 3: Commit**
```bash
git add web/src/hooks/use-api.ts
git commit -m "feat: update frontend api hooks for connectors"
```

---

### Task 4: Frontend - Build "Add Connector" Dialog

**Files:**
- Create: `web/src/components/channels/add-connector-dialog.tsx`
- Modify: `web/src/app/channels/page.tsx`

**Step 1: Create the dialog component**
Use shadcn/ui `Dialog`, `Form`, and `Input`.

**Step 2: Integrate into Channels page**
Add an "Add Bot" button to each channel card.

**Step 3: Verify UI**
Manual check: Dialog opens and form submits correctly.

**Step 4: Commit**
```bash
git add web/src/components/channels/add-connector-dialog.tsx web/src/app/channels/page.tsx
git commit -m "feat: add connector dialog UI"
```

---

### Task 5: Frontend - Implement Connector Deletion and Status

**Files:**
- Modify: `web/src/app/channels/page.tsx`

**Step 1: Add Delete button to connector list**
Add a trash icon/button next to each connector.

**Step 2: Implement status indicators**
Use the `useChannelStatus` hook to show real-time "Connected/Error" badges instead of just "Active/Inactive".

**Step 3: Commit**
```bash
git add web/src/app/channels/page.tsx
git commit -m "feat: implement connector deletion and status display"
```

---

### Task 6: Frontend - Add "Restart Required" Banner

**Files:**
- Modify: `web/src/app/channels/page.tsx`
- Create: `web/src/components/restart-banner.tsx`

**Step 1: Track configuration changes**
Use local state or a global flag to detect when a CRUD operation has succeeded.

**Step 2: Show banner**
Display a yellow banner at the top of the page when changes are made.

**Step 3: Commit**
```bash
git add web/src/components/restart-banner.tsx web/src/app/channels/page.tsx
git commit -m "feat: add restart required banner"
```

---

### Task 7: Verification and Cleanup

**Step 1: End-to-end manual test**
- Add a Telegram connector.
- Verify `config/main.yaml` is updated.
- Delete a connector.
- Verify `config/main.yaml` is updated.
- Check status indicators.

**Step 2: Run lsp_diagnostics**
Ensure no linting or type errors.

**Step 3: Final Commit**
```bash
git commit -m "chore: final cleanup and verification for multi-bot UI"
```
