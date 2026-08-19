SELECT COUNT(*) FROM salaries
SELECT gender, COUNT(*) FROM employees GROUP BY gender ORDER BY gender
SELECT MIN(hire_date), MAX(hire_date) FROM employees
SELECT d.dept_name, COUNT(*) AS n FROM dept_emp de JOIN departments d ON d.dept_no = de.dept_no GROUP BY d.dept_name ORDER BY n DESC, d.dept_name
SELECT YEAR(from_date), COUNT(*), MAX(salary) FROM salaries WHERE from_date >= '2000-01-01' GROUP BY YEAR(from_date) ORDER BY YEAR(from_date)
SELECT e.emp_no, e.last_name, s.salary FROM employees e JOIN salaries s ON s.emp_no = e.emp_no AND s.to_date = '9999-01-01' ORDER BY s.salary DESC, e.emp_no LIMIT 5
SELECT t.title, COUNT(DISTINCT t.emp_no), AVG(s.salary) FROM titles t JOIN salaries s ON s.emp_no = t.emp_no AND s.to_date = '9999-01-01' WHERE t.to_date = '9999-01-01' GROUP BY t.title ORDER BY t.title
SELECT COUNT(*) FROM dept_manager
