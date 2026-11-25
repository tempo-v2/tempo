CREATE TABLE coverage_view (
 report_id TEXT PRIMARY KEY REFERENCES coverage_report(id),
 header TEXT NOT NULL,
 studied_input TEXT NOT NULL,
 model_id TEXT NOT NULL
) STRICT;
CREATE TABLE coverage_gap_page (
 report_id TEXT NOT NULL REFERENCES coverage_report(id),
 page INTEGER NOT NULL,
 gaps TEXT NOT NULL,
 PRIMARY KEY(report_id,page)
) STRICT, WITHOUT ROWID;
