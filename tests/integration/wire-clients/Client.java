import java.nio.file.Files;
import java.nio.file.Path;
import java.sql.*;
import java.util.Properties;

class Client {
    static void require(boolean value, String message) {
        if (!value) throw new AssertionError(message);
    }

    public static void main(String[] args) throws Exception {
        String host = System.getenv().getOrDefault("PINTAIL_WIRE_HOST", "127.0.0.1");
        String port = System.getenv().getOrDefault("PINTAIL_WIRE_PORT", "3306");
        Properties properties = new Properties();
        properties.setProperty("user", "analytics");
        properties.setProperty("password", "pk_wire_secret");
        properties.setProperty("sslMode", "DISABLED");
        properties.setProperty("allowPublicKeyRetrieval", "true");
        properties.setProperty("useServerPrepStmts", "true");
        properties.setProperty("useInformationSchema", "true");
        properties.setProperty("connectTimeout", "10000");
        properties.setProperty("socketTimeout", "10000");
        try (Connection connection = DriverManager.getConnection("jdbc:mysql://" + host + ":" + port + "/analytics", properties)) {
            DatabaseMetaData metadata = connection.getMetaData();
            try (ResultSet tables = metadata.getTables("analytics", null, "events", new String[]{"TABLE"})) {
                require(tables.next() && tables.getString("TABLE_NAME").equals("events"), "table discovery");
            }
            try (ResultSet columns = metadata.getColumns("analytics", null, "events", "%")) {
                int count = 0;
                while (columns.next()) {
                    require(columns.getString("COLUMN_NAME") != null, "column name");
                    count++;
                }
                require(count == 2, "column discovery");
            }
            try (ResultSet keys = metadata.getPrimaryKeys("analytics", null, "events")) {
                require(keys.next() && keys.getString("COLUMN_NAME").equals("id"), "primary key discovery");
            }
            try (Statement statement = connection.createStatement()) {
                for (String sql : Files.readString(Path.of("metadata.sql")).split(";")) {
                    if (sql.isBlank()) continue;
                    try (ResultSet result = statement.executeQuery(sql)) {
                        require(result.getMetaData().getColumnCount() > 0, "metadata corpus columns");
                        while (result.next()) result.getObject(1);
                    }
                }
            }
            try (PreparedStatement statement = connection.prepareStatement("SELECT id, name FROM events WHERE id = ?")) {
                statement.setLong(1, 2);
                try (ResultSet result = statement.executeQuery()) {
                    require(result.next(), "prepared row");
                    require(result.getLong(1) == 2 && result.getString(2).equals("land"), "prepared values");
                }
            }
            try (PreparedStatement statement = connection.prepareStatement("SELECT CAST(1 AS DECIMAL(18,4)), CAST('2024-01-02 03:04:05.123456' AS DATETIME(6)), COUNT(*) FROM events")) {
                try (ResultSet result = statement.executeQuery()) {
                    ResultSetMetaData fields = result.getMetaData();
                    require(fields.getColumnType(1) == Types.DECIMAL && fields.getScale(1) == 4, "decimal metadata: type=" + fields.getColumnType(1) + " scale=" + fields.getScale(1));
                    require(fields.getColumnType(2) == Types.TIMESTAMP && fields.getPrecision(2) == 26, "datetime metadata: type=" + fields.getColumnType(2) + " precision=" + fields.getPrecision(2));
                    require(fields.isNullable(3) == ResultSetMetaData.columnNoNulls, "aggregate nullability");
                    require(result.next() && result.getLong(3) == 2, "aggregate result");
                    require(result.getTimestamp(2).getNanos() == 123456000, "datetime fraction");
                }
            }
        }
        System.out.println("JDBC-PASS");
    }
}
