package main

import (
	"database/sql"
	"encoding/json"
	"fmt"
	"os"

	_ "github.com/go-sql-driver/mysql"
)

func main() {
	host := os.Getenv("PINTAIL_WIRE_HOST")
	port := os.Getenv("PINTAIL_WIRE_PORT")
	dsn := fmt.Sprintf(
		"analytics:pk_wire_secret@tcp(%s:%s)/analytics?interpolateParams=true",
		host,
		port,
	)
	database, err := sql.Open("mysql", dsn)
	must(err)
	defer database.Close()
	database.SetMaxOpenConns(1)
	database.SetMaxIdleConns(1)

	var name string
	must(database.QueryRow("SELECT name FROM events WHERE id = ?", 2).Scan(&name))

	var columns uint64
	must(database.QueryRow(
		"SELECT COUNT(*) FROM information_schema.columns " +
			"WHERE table_schema = 'analytics' AND table_name = 'events'",
	).Scan(&columns))

	var tables uint64
	must(database.QueryRow(
		"SELECT COUNT(*) FROM information_schema.tables " +
			"WHERE table_schema = 'analytics' AND table_type = 'BASE TABLE'",
	).Scan(&tables))

	output, err := json.Marshal(map[string]any{
		"bound_name": name,
		"columns":    columns,
		"tables":     tables,
	})
	must(err)
	fmt.Println(string(output))
}

func must(err error) {
	if err != nil {
		panic(err)
	}
}
