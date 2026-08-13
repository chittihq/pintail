-- TPC-H schema, MySQL dialect.
--
-- The column names and types follow the TPC-H specification so the standard
-- queries run unmodified; the point of a recognised suite is that nobody has
-- to trust our translation of it.
--
-- InnoDB with declared foreign keys, because this is a CDC source: the shape
-- of the binlog depends on how the source stores and cascades, and a schema
-- that differs from a real one would exercise a replication path nobody uses.

CREATE TABLE region (
  r_regionkey   INT NOT NULL,
  r_name        CHAR(25) NOT NULL,
  r_comment     VARCHAR(152),
  PRIMARY KEY (r_regionkey)
) ENGINE=InnoDB;

CREATE TABLE nation (
  n_nationkey   INT NOT NULL,
  n_name        CHAR(25) NOT NULL,
  n_regionkey   INT NOT NULL,
  n_comment     VARCHAR(152),
  PRIMARY KEY (n_nationkey),
  KEY idx_nation_region (n_regionkey)
) ENGINE=InnoDB;

CREATE TABLE supplier (
  s_suppkey     INT NOT NULL,
  s_name        CHAR(25) NOT NULL,
  s_address     VARCHAR(40) NOT NULL,
  s_nationkey   INT NOT NULL,
  s_phone       CHAR(15) NOT NULL,
  s_acctbal     DECIMAL(15,2) NOT NULL,
  s_comment     VARCHAR(101) NOT NULL,
  PRIMARY KEY (s_suppkey),
  KEY idx_supplier_nation (s_nationkey)
) ENGINE=InnoDB;

CREATE TABLE part (
  p_partkey     INT NOT NULL,
  p_name        VARCHAR(55) NOT NULL,
  p_mfgr        CHAR(25) NOT NULL,
  p_brand       CHAR(10) NOT NULL,
  p_type        VARCHAR(25) NOT NULL,
  p_size        INT NOT NULL,
  p_container   CHAR(10) NOT NULL,
  p_retailprice DECIMAL(15,2) NOT NULL,
  p_comment     VARCHAR(23) NOT NULL,
  PRIMARY KEY (p_partkey)
) ENGINE=InnoDB;

CREATE TABLE partsupp (
  ps_partkey    INT NOT NULL,
  ps_suppkey    INT NOT NULL,
  ps_availqty   INT NOT NULL,
  ps_supplycost DECIMAL(15,2) NOT NULL,
  ps_comment    VARCHAR(199) NOT NULL,
  PRIMARY KEY (ps_partkey, ps_suppkey),
  KEY idx_partsupp_supp (ps_suppkey)
) ENGINE=InnoDB;

CREATE TABLE customer (
  c_custkey     INT NOT NULL,
  c_name        VARCHAR(25) NOT NULL,
  c_address     VARCHAR(40) NOT NULL,
  c_nationkey   INT NOT NULL,
  c_phone       CHAR(15) NOT NULL,
  c_acctbal     DECIMAL(15,2) NOT NULL,
  c_mktsegment  CHAR(10) NOT NULL,
  c_comment     VARCHAR(117) NOT NULL,
  PRIMARY KEY (c_custkey),
  KEY idx_customer_nation (c_nationkey)
) ENGINE=InnoDB;

CREATE TABLE orders (
  o_orderkey      INT NOT NULL,
  o_custkey       INT NOT NULL,
  o_orderstatus   CHAR(1) NOT NULL,
  o_totalprice    DECIMAL(15,2) NOT NULL,
  o_orderdate     DATE NOT NULL,
  o_orderpriority CHAR(15) NOT NULL,
  o_clerk         CHAR(15) NOT NULL,
  o_shippriority  INT NOT NULL,
  o_comment       VARCHAR(79) NOT NULL,
  PRIMARY KEY (o_orderkey),
  KEY idx_orders_cust (o_custkey),
  KEY idx_orders_date (o_orderdate)
) ENGINE=InnoDB;

CREATE TABLE lineitem (
  l_orderkey      INT NOT NULL,
  l_partkey       INT NOT NULL,
  l_suppkey       INT NOT NULL,
  l_linenumber    INT NOT NULL,
  l_quantity      DECIMAL(15,2) NOT NULL,
  l_extendedprice DECIMAL(15,2) NOT NULL,
  l_discount      DECIMAL(15,2) NOT NULL,
  l_tax           DECIMAL(15,2) NOT NULL,
  l_returnflag    CHAR(1) NOT NULL,
  l_linestatus    CHAR(1) NOT NULL,
  l_shipdate      DATE NOT NULL,
  l_commitdate    DATE NOT NULL,
  l_receiptdate   DATE NOT NULL,
  l_shipinstruct  CHAR(25) NOT NULL,
  l_shipmode      CHAR(10) NOT NULL,
  l_comment       VARCHAR(44) NOT NULL,
  PRIMARY KEY (l_orderkey, l_linenumber),
  KEY idx_lineitem_part (l_partkey),
  KEY idx_lineitem_supp (l_suppkey),
  KEY idx_lineitem_shipdate (l_shipdate)
) ENGINE=InnoDB;
