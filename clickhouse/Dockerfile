FROM clickhouse/clickhouse-server:24-alpine

# Copy our memory constraint configs into the config.d directory.
# ClickHouse merges every .xml file in config.d on top of the base config.
COPY memory.xml /etc/clickhouse-server/config.d/memory.xml
COPY users.xml  /etc/clickhouse-server/users.d/memory_users.xml
