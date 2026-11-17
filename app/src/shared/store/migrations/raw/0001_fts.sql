-- node_fts: the BM25 index behind `search` (`DATABASE.md` §3.5, `TECH-SPEC.md` §5.4.3).
--
-- External content over `node_fts_state`, not contentless and not a plain FTS table:
-- contentless cannot return the matched text, a plain table duplicates the whole corpus, and
-- the mirror makes a delete O(1) per row. `node_fts_state.rowid_` is declared
-- `INTEGER PRIMARY KEY`, so it *is* that table's rowid — which is what FTS5 joins on
-- (default `content_rowid = 'rowid'`), and the column names below are exactly the mirror's,
-- because FTS5 reads each column back from the content table by name.
--
-- `remove_diacritics 2` (not the default 1) folds Latin diacritics outside the legacy set as
-- well, so `cafe` matches `café`.
CREATE VIRTUAL TABLE node_fts USING fts5(
    name,
    qn,
    signature,
    docstring,
    props_text,
    content = 'node_fts_state',
    tokenize = 'unicode61 remove_diacritics 2'
);
