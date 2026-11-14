-- A repository's default branch: the branch the `origin` remote's HEAD names, as the local clone
-- records it (`refs/remotes/origin/HEAD`). Discovery observes it; it is protected like main and
-- master.
ALTER TABLE repository_sources ADD COLUMN default_ref TEXT
 CHECK(default_ref IS NULL OR length(default_ref) BETWEEN 1 AND 1024);
