CREATE TABLE filenodes (
  repo_id INT UNSIGNED NOT NULL,
  path_hash VARBINARY(32) NOT NULL,
  -- Fetching of a BIT field for Mysql Diesel backend fails with
  -- "Numeric overflow/underflow occurred". So use TINYINT instead
  is_tree TINYINT NOT NULL,
  filenode BINARY(20) NOT NULL,
  linknode VARBINARY(32) NOT NULL,
  p1 BINARY(20),
  p2 BINARY(20),
  -- Fetching of a BIT field for Mysql Diesel backend fails with
  -- "Numeric overflow/underflow occurred". So use TINYINT instead
  has_copyinfo TINYINT NOT NULL,
  PRIMARY KEY (repo_id, path_hash, is_tree, filenode)
);

CREATE TABLE fixedcopyinfo (
  repo_id INT UNSIGNED NOT NULL,
  topath_hash VARBINARY(32) NOT NULL,
  tonode BINARY(20) NOT NULL,
  is_tree TINYINT NOT NULL,
  frompath_hash VARBINARY(32) NOT NULL,
  fromnode BINARY(20) NOT NULL,
  PRIMARY KEY (repo_id, topath_hash, tonode, is_tree)
);

CREATE TABLE paths (
  repo_id INT UNSIGNED NOT NULL,
  path_hash VARBINARY(32) NOT NULL,
  path VARBINARY(4096) NOT NULL,
  PRIMARY KEY (repo_id, path_hash)
);
