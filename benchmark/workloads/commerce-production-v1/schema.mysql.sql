-- commerce-production-v1: production-shaped multi-tenant commerce schema.
-- Statuses are separated (order / payment / fulfillment), line items preserve
-- transactional values, orders preserve checkout shipping region, soft deletes
-- via deleted_at, currencies explicit. shipment_items -> shipments carries a
-- deliberate ON DELETE CASCADE as the CDC negative-control case (cascaded
-- deletes are invisible in the binlog; the probe must flag it).

CREATE TABLE tenants (
  id              BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  external_id     BINARY(16) NOT NULL,
  name            VARCHAR(128) NOT NULL,
  plan            VARCHAR(32) NOT NULL,
  country         CHAR(2) NOT NULL,
  created_at      DATETIME(6) NOT NULL,
  updated_at      DATETIME(6) NOT NULL,
  deleted_at      DATETIME(6),
  PRIMARY KEY (id),
  UNIQUE KEY uq_tenants_external (external_id)
) ENGINE=InnoDB;

CREATE TABLE customers (
  id              BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  external_id     BINARY(16) NOT NULL,
  tenant_id       BIGINT UNSIGNED NOT NULL,
  email           VARCHAR(255) NOT NULL,
  full_name       VARCHAR(255) NOT NULL,
  locale          VARCHAR(12),
  marketing_opt_in TINYINT(1) NOT NULL DEFAULT 0,
  lifetime_value  DECIMAL(18,4) NOT NULL DEFAULT 0,
  created_at      DATETIME(6) NOT NULL,
  updated_at      DATETIME(6) NOT NULL,
  deleted_at      DATETIME(6),
  PRIMARY KEY (id),
  UNIQUE KEY uq_customers_external (external_id),
  KEY idx_customers_tenant (tenant_id, created_at)
) ENGINE=InnoDB;

CREATE TABLE customer_addresses (
  id              BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  customer_id     BIGINT UNSIGNED NOT NULL,
  tenant_id       BIGINT UNSIGNED NOT NULL,
  kind            VARCHAR(16) NOT NULL,          -- billing | shipping
  country         CHAR(2) NOT NULL,
  region          VARCHAR(64),
  city            VARCHAR(128) NOT NULL,
  postal_code     VARCHAR(16),
  is_default      TINYINT(1) NOT NULL DEFAULT 0,
  created_at      DATETIME(6) NOT NULL,
  updated_at      DATETIME(6) NOT NULL,
  PRIMARY KEY (id),
  KEY idx_addresses_customer (customer_id)
) ENGINE=InnoDB;

CREATE TABLE categories (
  id              BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  parent_id       BIGINT UNSIGNED,
  name            VARCHAR(128) NOT NULL,
  path            VARCHAR(512) NOT NULL,
  created_at      DATETIME(6) NOT NULL,
  updated_at      DATETIME(6) NOT NULL,
  PRIMARY KEY (id),
  KEY idx_categories_parent (parent_id)
) ENGINE=InnoDB;

CREATE TABLE products (
  id              BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  external_id     BINARY(16) NOT NULL,
  tenant_id       BIGINT UNSIGNED NOT NULL,
  category_id     BIGINT UNSIGNED NOT NULL,
  name            VARCHAR(255) NOT NULL,
  description     TEXT,
  brand           VARCHAR(128),
  created_at      DATETIME(6) NOT NULL,
  updated_at      DATETIME(6) NOT NULL,
  deleted_at      DATETIME(6),
  PRIMARY KEY (id),
  UNIQUE KEY uq_products_external (external_id),
  KEY idx_products_tenant_category (tenant_id, category_id)
) ENGINE=InnoDB;

CREATE TABLE product_variants (
  id              BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  product_id      BIGINT UNSIGNED NOT NULL,
  tenant_id       BIGINT UNSIGNED NOT NULL,
  sku             VARCHAR(64) NOT NULL,
  attributes      JSON,
  currency        CHAR(3) NOT NULL,
  list_price      DECIMAL(18,4) NOT NULL,
  cost_price      DECIMAL(18,4),
  weight_grams    INT UNSIGNED,
  created_at      DATETIME(6) NOT NULL,
  updated_at      DATETIME(6) NOT NULL,
  deleted_at      DATETIME(6),
  PRIMARY KEY (id),
  UNIQUE KEY uq_variants_sku (tenant_id, sku),
  KEY idx_variants_product (product_id)
) ENGINE=InnoDB;

CREATE TABLE warehouses (
  id              BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  tenant_id       BIGINT UNSIGNED NOT NULL,
  code            VARCHAR(16) NOT NULL,
  name            VARCHAR(128) NOT NULL,
  country         CHAR(2) NOT NULL,
  region          VARCHAR(64),
  created_at      DATETIME(6) NOT NULL,
  updated_at      DATETIME(6) NOT NULL,
  PRIMARY KEY (id),
  UNIQUE KEY uq_warehouses_code (tenant_id, code)
) ENGINE=InnoDB;

CREATE TABLE inventory_balances (
  id              BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  tenant_id       BIGINT UNSIGNED NOT NULL,
  variant_id      BIGINT UNSIGNED NOT NULL,
  warehouse_id    BIGINT UNSIGNED NOT NULL,
  on_hand         INT NOT NULL,
  reserved        INT NOT NULL,
  reorder_point   INT,
  updated_at      DATETIME(6) NOT NULL,
  PRIMARY KEY (id),
  UNIQUE KEY uq_inventory (variant_id, warehouse_id),
  KEY idx_inventory_tenant (tenant_id, warehouse_id)
) ENGINE=InnoDB;

CREATE TABLE orders (
  id                  BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  external_id         BINARY(16) NOT NULL,
  tenant_id           BIGINT UNSIGNED NOT NULL,
  customer_id         BIGINT UNSIGNED NOT NULL,
  currency            CHAR(3) NOT NULL,
  subtotal_amount     DECIMAL(18,4) NOT NULL,
  discount_amount     DECIMAL(18,4) NOT NULL,
  tax_amount          DECIMAL(18,4) NOT NULL,
  shipping_amount     DECIMAL(18,4) NOT NULL,
  total_amount        DECIMAL(18,4) NOT NULL,
  order_status        VARCHAR(32) NOT NULL,     -- pending|confirmed|completed|cancelled
  payment_status      VARCHAR(32) NOT NULL,     -- pending|authorized|paid|failed|refunded
  fulfillment_status  VARCHAR(32) NOT NULL,     -- unfulfilled|partial|fulfilled|returned
  shipping_country    CHAR(2) NOT NULL,
  shipping_region     VARCHAR(64),
  sales_channel       VARCHAR(32) NOT NULL,     -- web|mobile|pos|api|marketplace
  promotion_code      VARCHAR(64),
  metadata            JSON,
  placed_at           DATETIME(6) NOT NULL,
  cancelled_at        DATETIME(6),
  completed_at        DATETIME(6),
  updated_at          DATETIME(6) NOT NULL,
  deleted_at          DATETIME(6),
  PRIMARY KEY (id),
  UNIQUE KEY uq_orders_external (external_id),
  KEY idx_orders_tenant_placed (tenant_id, placed_at),
  KEY idx_orders_customer_placed (customer_id, placed_at),
  KEY idx_orders_fulfillment (tenant_id, fulfillment_status, placed_at),
  KEY idx_orders_payment (tenant_id, payment_status, placed_at)
) ENGINE=InnoDB;

CREATE TABLE order_items (
  id                  BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  order_id            BIGINT UNSIGNED NOT NULL,
  tenant_id           BIGINT UNSIGNED NOT NULL,
  product_variant_id  BIGINT UNSIGNED NOT NULL,
  sku                 VARCHAR(64) NOT NULL,
  product_name        VARCHAR(255) NOT NULL,
  quantity            INT UNSIGNED NOT NULL,
  unit_price          DECIMAL(18,4) NOT NULL,
  discount_amount     DECIMAL(18,4) NOT NULL,
  tax_amount          DECIMAL(18,4) NOT NULL,
  total_amount        DECIMAL(18,4) NOT NULL,
  created_at          DATETIME(6) NOT NULL,
  updated_at          DATETIME(6) NOT NULL,
  PRIMARY KEY (id),
  KEY idx_items_order (order_id),
  KEY idx_items_tenant_variant (tenant_id, product_variant_id)
) ENGINE=InnoDB;

CREATE TABLE payments (
  id              BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  order_id        BIGINT UNSIGNED NOT NULL,
  tenant_id       BIGINT UNSIGNED NOT NULL,
  attempt         INT UNSIGNED NOT NULL,
  provider        VARCHAR(32) NOT NULL,          -- stripe|adyen|paypal|razorpay|cod
  method          VARCHAR(32) NOT NULL,          -- card|upi|netbanking|wallet|cod
  status          VARCHAR(32) NOT NULL,          -- pending|authorized|captured|failed|voided
  failure_code    VARCHAR(64),
  currency        CHAR(3) NOT NULL,
  amount          DECIMAL(18,4) NOT NULL,
  provider_ref    VARCHAR(128),
  created_at      DATETIME(6) NOT NULL,
  updated_at      DATETIME(6) NOT NULL,
  PRIMARY KEY (id),
  KEY idx_payments_order (order_id),
  KEY idx_payments_tenant_status (tenant_id, status, created_at)
) ENGINE=InnoDB;

CREATE TABLE refunds (
  id              BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  order_id        BIGINT UNSIGNED NOT NULL,
  payment_id      BIGINT UNSIGNED NOT NULL,
  tenant_id       BIGINT UNSIGNED NOT NULL,
  reason          VARCHAR(64) NOT NULL,          -- damaged|wrong_item|late|customer_request|fraud
  status          VARCHAR(32) NOT NULL,          -- pending|approved|processed|rejected
  currency        CHAR(3) NOT NULL,
  amount          DECIMAL(18,4) NOT NULL,        -- partial refunds allowed
  created_at      DATETIME(6) NOT NULL,
  updated_at      DATETIME(6) NOT NULL,
  PRIMARY KEY (id),
  KEY idx_refunds_order (order_id),
  KEY idx_refunds_tenant_created (tenant_id, created_at)
) ENGINE=InnoDB;

CREATE TABLE shipments (
  id              BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  order_id        BIGINT UNSIGNED NOT NULL,
  tenant_id       BIGINT UNSIGNED NOT NULL,
  warehouse_id    BIGINT UNSIGNED NOT NULL,
  carrier         VARCHAR(32) NOT NULL,
  tracking_code   VARCHAR(64),
  status          VARCHAR(32) NOT NULL,          -- pending|picked|in_transit|delivered|lost
  shipped_at      DATETIME(6),
  delivered_at    DATETIME(6),
  created_at      DATETIME(6) NOT NULL,
  updated_at      DATETIME(6) NOT NULL,
  PRIMARY KEY (id),
  KEY idx_shipments_order (order_id),
  KEY idx_shipments_tenant_status (tenant_id, status, created_at)
) ENGINE=InnoDB;

-- NEGATIVE CONTROL: cascaded deletes here never appear in the binlog.
CREATE TABLE shipment_items (
  id              BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  shipment_id     BIGINT UNSIGNED NOT NULL,
  order_item_id   BIGINT UNSIGNED NOT NULL,
  tenant_id       BIGINT UNSIGNED NOT NULL,
  quantity        INT UNSIGNED NOT NULL,
  created_at      DATETIME(6) NOT NULL,
  PRIMARY KEY (id),
  KEY idx_shipment_items_shipment (shipment_id),
  CONSTRAINT fk_shipment_items_shipment
    FOREIGN KEY (shipment_id) REFERENCES shipments (id) ON DELETE CASCADE
) ENGINE=InnoDB;

CREATE TABLE order_events (
  id              BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  order_id        BIGINT UNSIGNED NOT NULL,
  tenant_id       BIGINT UNSIGNED NOT NULL,
  event_type      VARCHAR(64) NOT NULL,
  actor           VARCHAR(32) NOT NULL,          -- system|customer|operator|webhook
  payload         JSON,
  created_at      DATETIME(6) NOT NULL,
  PRIMARY KEY (id),
  KEY idx_events_order (order_id),
  KEY idx_events_tenant_type_created (tenant_id, event_type, created_at)
) ENGINE=InnoDB;

-- Sentinel table for measuring source-to-visible replication lag. A row is
-- written to MySQL and polled for on the replica, so the recorded lag is the
-- time replication took rather than the time the harness chose to wait.
--
-- Deliberately trivial: no indexes to build, no width to decode, so what it
-- measures is the pipeline's latency and not the cost of the row.
CREATE TABLE lag_probe (
  id              BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  marker          VARCHAR(64) NOT NULL,
  PRIMARY KEY (id),
  KEY idx_lag_probe_marker (marker)
) ENGINE=InnoDB;
