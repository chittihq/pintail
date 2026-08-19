SELECT rating, COUNT(*) FROM film GROUP BY rating ORDER BY rating
SELECT COUNT(*) FROM film WHERE rating > 'PG'
SELECT MIN(rating), MAX(rating) FROM film
SELECT special_features, COUNT(*) FROM film GROUP BY special_features ORDER BY special_features LIMIT 10
SELECT special_features, COUNT(*) FROM film GROUP BY special_features ORDER BY COUNT(*) DESC, special_features LIMIT 6
SELECT release_year, COUNT(*) FROM film GROUP BY release_year ORDER BY release_year
SELECT c.name, COUNT(*) AS n FROM film_category fc JOIN category c ON c.category_id = fc.category_id GROUP BY c.name ORDER BY n DESC, c.name LIMIT 5
SELECT s.store_id, SUM(p.amount) FROM payment p JOIN staff s ON s.staff_id = p.staff_id GROUP BY s.store_id ORDER BY s.store_id
SELECT DATE(rental_date), COUNT(*) FROM rental WHERE rental_date >= '2005-08-20 00:00:00' GROUP BY DATE(rental_date) ORDER BY DATE(rental_date)
SELECT a.actor_id, a.last_name, COUNT(*) FROM actor a JOIN film_actor fa ON fa.actor_id = a.actor_id GROUP BY a.actor_id, a.last_name ORDER BY COUNT(*) DESC, a.actor_id LIMIT 5
SELECT c.customer_id, SUM(p.amount) AS total FROM customer c JOIN payment p ON p.customer_id = c.customer_id GROUP BY c.customer_id ORDER BY total DESC, c.customer_id LIMIT 5
SELECT f.film_id, f.title FROM film f WHERE NOT EXISTS (SELECT 1 FROM inventory i WHERE i.film_id = f.film_id) ORDER BY f.film_id LIMIT 10
SELECT l.name, COUNT(*) FROM film f JOIN language l ON l.language_id = f.language_id GROUP BY l.name ORDER BY l.name
