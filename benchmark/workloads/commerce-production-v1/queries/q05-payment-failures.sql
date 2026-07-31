-- class: risk-analytics
-- Daily payment failure rate and top failure codes over the trailing 30 days.
SELECT
  DATE(created_at) AS day,
  provider,
  COUNT(*) AS attempts,
  SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END) AS failures,
  SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END) / COUNT(*) AS failure_rate,
  SUM(CASE WHEN status = 'failed' AND failure_code = 'insufficient_funds' THEN 1 ELSE 0 END) AS insufficient_funds,
  SUM(CASE WHEN status = 'failed' AND failure_code = 'card_declined' THEN 1 ELSE 0 END) AS card_declined
FROM payments
WHERE created_at >= :now - INTERVAL 30 DAY
GROUP BY DATE(created_at), provider
ORDER BY day DESC, provider;
