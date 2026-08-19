SELECT Continent, COUNT(*), SUM(Population) FROM country GROUP BY Continent ORDER BY Continent
SELECT c.Name, ci.Name, ci.Population FROM city ci JOIN country c ON c.Code = ci.CountryCode ORDER BY ci.Population DESC, ci.ID LIMIT 5
SELECT cl.Language, COUNT(*), SUM(c.Population * cl.Percentage / 100) FROM countrylanguage cl JOIN country c ON c.Code = cl.CountryCode WHERE cl.IsOfficial = 'T' GROUP BY cl.Language ORDER BY COUNT(*) DESC, cl.Language LIMIT 8
SELECT Region, AVG(LifeExpectancy), MIN(IndepYear) FROM country WHERE LifeExpectancy IS NOT NULL GROUP BY Region ORDER BY Region LIMIT 10
