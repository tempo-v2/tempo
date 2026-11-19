// Disposable acceptance fixtures use this complete readback before and after detection.
import { DatabaseSync } from "node:sqlite";

export function databaseRows(database) {
  const db = new DatabaseSync(database, { readOnly: true });
  try {
    db.exec("PRAGMA busy_timeout=3000; BEGIN DEFERRED");
    return db.prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
      .all().map(({ name }) => {
        const identifier = '"' + name.replaceAll('"', '""') + '"';
        return [name, db.prepare('SELECT * FROM ' + identifier).all()];
      });
  } finally {
    db.close();
  }
}
