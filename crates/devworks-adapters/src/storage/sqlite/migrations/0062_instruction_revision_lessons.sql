-- The lessons a skill or role revision came from. A draft names them; the revision's history and
-- the lesson's own view both show the link. A link is as old as its revision and never changes:
-- revisions are immutable, so the rows are only ever inserted with the revision they describe.
-- Both ends must exist: the revision row, and the Knowledge claim that holds the lesson.
CREATE TABLE instruction_revision_lessons (
    instruction_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK(revision > 0),
    lesson_id TEXT NOT NULL REFERENCES knowledge_claims(id),
    PRIMARY KEY(instruction_id, revision, lesson_id),
    FOREIGN KEY(instruction_id, revision) REFERENCES instruction_revisions(instruction_id, revision)
) STRICT, WITHOUT ROWID;
-- The lesson's view lists the revisions that name it.
CREATE INDEX instruction_revision_lessons_by_lesson ON instruction_revision_lessons(lesson_id, instruction_id, revision);
