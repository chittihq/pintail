-- ---------------------------------------------------------------------------
-- Seed for the Pintail differential conformance corpus.
--
-- Apply to the SOURCE MySQL, let Pintail replicate, then run:
--   bun run conformance:pintail
--
-- The data is deliberately adversarial: case variants, trailing spaces,
-- accents, NULL join keys, ties on the sort column, an ENUM whose declared
-- order is NOT alphabetical, and columns with differing collations.
-- ---------------------------------------------------------------------------

DROP DATABASE IF EXISTS conformance_seed;
CREATE DATABASE conformance_seed CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci;
USE conformance_seed;

-- Joined three times under different aliases by join-01/02.
CREATE TABLE Person (
  personId INT NOT NULL PRIMARY KEY,
  name     VARCHAR(64) NOT NULL
) ENGINE=InnoDB;

-- `code` and `label` carry DIFFERENT collations on purpose: grouping both at
-- once is what previously failed. `status` is an ENUM whose declared order is
-- not alphabetical, so ordinal and label sorting disagree.
CREATE TABLE Dim (
  dimId  INT NOT NULL PRIMARY KEY,
  code   VARCHAR(32) COLLATE utf8mb4_0900_ai_ci NOT NULL,
  label  VARCHAR(64) COLLATE utf8mb4_general_ci NOT NULL,
  padded VARCHAR(32) COLLATE utf8mb4_0900_ai_ci NOT NULL,
  status ENUM('zebra','active','beta') NOT NULL
) ENGINE=InnoDB;

CREATE TABLE Fact (
  factId        INT NOT NULL PRIMARY KEY,
  dimId         INT NOT NULL,
  nullableDimId INT NULL,
  code          VARCHAR(32) COLLATE utf8mb4_0900_ai_ci NOT NULL,
  amount        DECIMAL(12,2) NOT NULL,
  createdBy     INT NULL,
  updatedBy     INT NULL,
  ownedBy       INT NULL,
  effectiveFrom DATE NULL,
  createdAt     DATETIME NOT NULL,
  KEY idx_dim (dimId)
) ENGINE=InnoDB;

CREATE TABLE Event (
  eventId INT NOT NULL PRIMARY KEY,
  dimId   INT NOT NULL,
  at      DATETIME NOT NULL,
  KEY idx_dim (dimId)
) ENGINE=InnoDB;

INSERT INTO Person (personId, name) VALUES
  (1,'Alice'), (2,'Bob'), (3,'Carol');

-- Case variants (alpha/Alpha/ALPHA) and trailing-space variants: an engine that
-- folds these differently from MySQL changes the group count.
INSERT INTO Dim (dimId, code, label, padded, status) VALUES
  (1,'alpha','Alpha','pad',    'active'),
  (2,'beta', 'alpha','pad ',   'zebra'),
  (3,'Alpha','ALPHA','pad  ',  'beta'),
  (4,'gamma','Gamma','padx',   'active'),
  (5,'ALPHA','gamma','padx ',  'zebra');

-- createdBy=2 exists; updatedBy=99 does NOT exist, so the second alias must
-- NULL-extend rather than borrow the first alias's row.
-- createdAt has ties on purpose (factId 3/4/5 share a timestamp).
INSERT INTO Fact (factId, dimId, nullableDimId, code, amount, createdBy, updatedBy, ownedBy, effectiveFrom, createdAt) VALUES
  (1, 1, 1,    'alpha',  10.00, 2, 99,   1,    '2025-07-01', '2025-07-21 04:15:00'),
  (2, 1, NULL, 'Alpha',  20.50, 1, 2,    NULL, NULL,         '2025-07-21 23:45:00'),
  (3, 2, NULL, 'beta',   -5.25, 3, 99,   2,    '2025-08-01', '2025-07-22 09:00:00'),
  (4, 2, 2,    'BETA',    0.00, 99, 1,   3,    '2025-01-01', '2025-07-22 09:00:00'),
  (5, 3, NULL, 'gamma', 100.75, NULL, NULL, NULL, NULL,      '2025-07-22 09:00:00'),
  (6, 4, 4,    'gamma',  42.00, 1, 3,    2,    '2025-12-31', '2025-07-25 06:00:00'),
  (7, 5, NULL, 'ALPHA',   7.10, 2, 1,    1,    '2025-07-15', '2025-07-27 11:11:11');

-- dimId 5 has no events, so anti-join and outer-join NULL-extension are exercised.
INSERT INTO Event (eventId, dimId, at) VALUES
  (1, 1, '2025-06-01 00:00:00'),
  (2, 1, '2025-07-10 00:00:00'),
  (3, 2, '2025-09-01 00:00:00'),
  (4, 3, '2024-01-01 00:00:00'),
  (5, 4, '2026-01-01 00:00:00');
