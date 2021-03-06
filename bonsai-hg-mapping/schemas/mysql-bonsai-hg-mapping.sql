CREATE TABLE bonsai_hg_mapping (
  repo_id INTEGER NOT NULL,
  hg_cs_id BINARY(20) NOT NULL,
  bcs_id BINARY(32) NOT NULL,
  UNIQUE hg_cs_id_unique_per_repo (repo_id, hg_cs_id),
  PRIMARY KEY (repo_id, bcs_id)
);
