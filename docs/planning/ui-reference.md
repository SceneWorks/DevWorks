# Approved UI reference

The approved revision 08 design keeps Chat as the primary app-wide interface and Overview specific to a project. This reference consolidates the user-reviewed presentation from 2 November 2026. Numbered requirements in [the requirements reader](devworks-requirements.html), the [architecture](../architecture/devworks-architecture.md), and CodeGraph delivery criteria carry the same decisions.

The interactive HTML study uses local sample records. Its browser checks and screenshots validate the prototype; production delivery remains native Rust and egui, with native acceptance still required. This document describes the current design, superseding earlier screenshots and accumulated trial notes where they differ.

## Chat and its shared coordinator

The left navigation remains visible, the active workspace conversation occupies the center, and the right sidebar has **Current** first and **Chat History** second. Current is selected initially. It answers app-wide status questions through Needs you, Working and Recently completed, with project labels and direct work links. Overview remains project specific.

Chat History retains search, date grouping and the active conversation selection. Switching tabs preserves the conversation, draft, pending attachments and relevant scroll positions. Selecting a conversation restores its transcript and draft. **New Chat** stays in the center header beside **Coordinator instructions**, reachable with either sidebar tab selected. Omit the redundant conversation-name header row and the center “Across your projects” strip.

The toolbar chat icon opens the same selected workspace conversation beside other screens. It keeps its session, messages, draft, attachments and model pin, remains open across navigation, and closes explicitly or on returning to Chat. Dedicated feature/issue conversations retain their own identity.

Screen context follows COORD-09: opening the coordinator sidebar captures the current screen; meaningful navigation captures another notice only while it is open. Closed browsing and returning to Chat append none, and reopening never replays missed screens. Both views display only the latest recorded notice per conversation in its original position. Earlier notices remain unchanged in transcript/provider history, with ordinary messages preserved in order. Navigation performs no inference or work authorization.

## Navigation and project work

- Place global **Approvals** directly below Chat. Its inbox spans projects and supports project filtering.
- Put **Activity** within project Overview, including filters and full history; remove its separate left-navigation destination.
- Keep Intelligence destinations accessible through a collapsible navigation group.
- Left-align the installed version and update message at the bottom of the left sidebar. The update message retains its action.

## Completion notifications

Verified feature and issue completions produce floating cards at the top center below the global toolbar, consistently on Chat and other screens. The overlay reserves no space, shifts no content, and leaves essential controls usable. It does not take focus.

Cards stack like playing cards, newest first, and can expand to expose all messages. Every card persists until explicitly dismissed, including across navigation and restart. Opening or reading completed work does not dismiss it. Dismiss cards individually and retain their records in notification history. Include direct links to completed work and suppress duplicate completion events. Preserve keyboard dismissal/access, accessible status announcements and reduced motion. Reading or dismissing a notification does not resolve an approval or resume work. Required-input OS alerts remain a separate policy under NOTIFY-04.

## Headers, surfaces and sidebars

Use the Workspace coordinator / Overview pattern across all pages and secondary views: warm neutral surfaces, bordered groups, consistent type and spacing, and quieter secondary actions. Each header belongs **inside its primary content pane**, sharing the pane’s continuous outer border with a divider into its body. A separate full-width header card does not match this design. Secondary panes begin alongside the integrated header.

| Workspace | Inner sidebar placement |
| --- | --- |
| Approvals | Right of primary content |
| Knowledge entries and documents | Right of primary content |
| Dreaming run history | Right of primary content |
| Databases | Right of primary content |

Apply the same complete treatment to Repositories, Agents, Artifacts, Knowledge, Dreaming and every Database view/state. Keep artifact previews contained above their metadata and show honest empty/error/stale states. Preserve existing controls, forms, selection, filters and scroll positions. Responsive pane widths may adapt; do not reintroduce detached headers.

Overview and Features use the same compact summary strip directly beneath their integrated headers. Numbers and labels sit beside each other with a shared baseline, size and spacing.

## Composer

Use a **plus-only attachment button** with the tooltip and accessible name “Attach images or files.” File selection, drag/drop and paste, pending attachment removal and durable message association remain supported across interactive chat composers.

In the workspace composer, align the plus button, model/tier selector and effort selector with Send along the bottom of the input. Below the input border, show **All projects** followed by the pinned model on the left and **⌘ / Ctrl + Enter to send** on the right. Omit “Workspace coordinator” from this metadata. Apply the same arrangement in the shared sidebar, allowing controls/metadata to wrap at narrower widths. Routing, effort, pinning, sending and attachment behavior remain intact.

## Requirement and delivery mapping

| Approved behavior | Requirements | Existing native delivery slices |
| --- | --- | --- |
| Chat tabs, center New Chat and shared conversation | UI-01–04, UI-11–13 | DW-10, DW-16 |
| Composer and attachments | UI-02, UI-08–10 | DW-10, DW-16 |
| Global Approvals, Overview Activity and navigation | UI-14 | DW-10, DW-12, DW-14 |
| Integrated headers, panels, inner sidebars and summary/footer alignment | UI-15 | DW-10, DW-12, DW-16 |
| Persistent floating completion cards | NOTIFY-01–05 | DW-10, DW-14 |
| Screen context capture and display projection | COORD-09–10 | DW-15, DW-16 |

See the [interactive prototype](harness-workbench.html?page=chat&revision=8&project=sw), [screen gallery](mockup-gallery.html), and [prototype verification](ui-08/verification.json). They illustrate the approved design and do not replace native implementation evidence.
