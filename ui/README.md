# Tempo UI

Build Core first, then install dependencies here and run `npm run build`. The host serves `dist/index.html` and `dist/assets/`. The browser posts commands to `/api/study`; all application errors are decoded Study views. `study_controller.js` is the sole session controller; transport and storage are injected. The production storage envelope is `{pending: AnswerCommand | AbandonCommand | null, lastAttempt: string | null}` in sessionStorage. A pending command is persisted before delivery and replayed exactly on reload. Resolution must also be persisted before a new intent.

The page receives a view and actions. PuzzleBoard translates all supplied legal UCI moves into destinations and preserves the backend identity and revision. Promotion remains local until a piece is selected and is fenced against the originating position. The browser does not grade or persist chess state.
