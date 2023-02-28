CREATE TABLE supplier (s_suppkey INT, s_name CHARACTER VARYING, s_address CHARACTER VARYING, s_nationkey INT, s_phone CHARACTER VARYING, s_acctbal NUMERIC, s_comment CHARACTER VARYING, PRIMARY KEY (s_suppkey));
CREATE TABLE part (p_partkey INT, p_name CHARACTER VARYING, p_mfgr CHARACTER VARYING, p_brand CHARACTER VARYING, p_type CHARACTER VARYING, p_size INT, p_container CHARACTER VARYING, p_retailprice NUMERIC, p_comment CHARACTER VARYING, PRIMARY KEY (p_partkey));
CREATE TABLE partsupp (ps_partkey INT, ps_suppkey INT, ps_availqty INT, ps_supplycost NUMERIC, ps_comment CHARACTER VARYING, PRIMARY KEY (ps_partkey, ps_suppkey));
CREATE TABLE customer (c_custkey INT, c_name CHARACTER VARYING, c_address CHARACTER VARYING, c_nationkey INT, c_phone CHARACTER VARYING, c_acctbal NUMERIC, c_mktsegment CHARACTER VARYING, c_comment CHARACTER VARYING, PRIMARY KEY (c_custkey));
CREATE TABLE orders (o_orderkey BIGINT, o_custkey INT, o_orderstatus CHARACTER VARYING, o_totalprice NUMERIC, o_orderdate DATE, o_orderpriority CHARACTER VARYING, o_clerk CHARACTER VARYING, o_shippriority INT, o_comment CHARACTER VARYING, PRIMARY KEY (o_orderkey));
CREATE TABLE lineitem (l_orderkey BIGINT, l_partkey INT, l_suppkey INT, l_linenumber INT, l_quantity NUMERIC, l_extendedprice NUMERIC, l_discount NUMERIC, l_tax NUMERIC, l_returnflag CHARACTER VARYING, l_linestatus CHARACTER VARYING, l_shipdate DATE, l_commitdate DATE, l_receiptdate DATE, l_shipinstruct CHARACTER VARYING, l_shipmode CHARACTER VARYING, l_comment CHARACTER VARYING, PRIMARY KEY (l_orderkey, l_linenumber));
CREATE TABLE nation (n_nationkey INT, n_name CHARACTER VARYING, n_regionkey INT, n_comment CHARACTER VARYING, PRIMARY KEY (n_nationkey));
CREATE TABLE region (r_regionkey INT, r_name CHARACTER VARYING, r_comment CHARACTER VARYING, PRIMARY KEY (r_regionkey));
CREATE TABLE person (id BIGINT, name CHARACTER VARYING, email_address CHARACTER VARYING, credit_card CHARACTER VARYING, city CHARACTER VARYING, state CHARACTER VARYING, date_time TIMESTAMP, extra CHARACTER VARYING, PRIMARY KEY (id));
CREATE TABLE auction (id BIGINT, item_name CHARACTER VARYING, description CHARACTER VARYING, initial_bid BIGINT, reserve BIGINT, date_time TIMESTAMP, expires TIMESTAMP, seller BIGINT, category BIGINT, extra CHARACTER VARYING, PRIMARY KEY (id));
CREATE TABLE bid (auction BIGINT, bidder BIGINT, price BIGINT, channel CHARACTER VARYING, url CHARACTER VARYING, date_time TIMESTAMP, extra CHARACTER VARYING);
CREATE TABLE alltypes1 (c1 BOOLEAN, c2 SMALLINT, c3 INT, c4 BIGINT, c5 REAL, c6 DOUBLE, c7 NUMERIC, c8 DATE, c9 CHARACTER VARYING, c10 TIME, c11 TIMESTAMP, c13 INTERVAL, c14 STRUCT<a INT>, c15 INT[], c16 CHARACTER VARYING[]);
CREATE TABLE alltypes2 (c1 BOOLEAN, c2 SMALLINT, c3 INT, c4 BIGINT, c5 REAL, c6 DOUBLE, c7 NUMERIC, c8 DATE, c9 CHARACTER VARYING, c10 TIME, c11 TIMESTAMP, c13 INTERVAL, c14 STRUCT<a INT>, c15 INT[], c16 CHARACTER VARYING[]);
CREATE MATERIALIZED VIEW m0 AS SELECT hop_0.url AS col_0, 'oIS5Tt7wUO' AS col_1 FROM hop(bid, bid.date_time, INTERVAL '3600', INTERVAL '316800') AS hop_0 GROUP BY hop_0.url;
CREATE MATERIALIZED VIEW m1 AS SELECT (INT '686') AS col_0, (- (REAL '591')) AS col_1, t_0.ps_partkey AS col_2 FROM partsupp AS t_0 LEFT JOIN part AS t_1 ON t_0.ps_comment = t_1.p_type WHERE false GROUP BY t_1.p_container, t_1.p_partkey, t_0.ps_comment, t_1.p_retailprice, t_0.ps_partkey;
CREATE MATERIALIZED VIEW m2 AS WITH with_0 AS (SELECT (INTERVAL '1') AS col_0, (BIGINT '945') AS col_1, (TIME '06:35:55' IS NOT NULL) AS col_2 FROM alltypes1 AS t_1 FULL JOIN supplier AS t_2 ON t_1.c3 = t_2.s_nationkey AND t_1.c1 GROUP BY t_1.c1, t_1.c7, t_2.s_name, t_2.s_address) SELECT (SMALLINT '659') AS col_0, DATE '2022-09-05' AS col_1, (BIGINT '715') AS col_2, (BIGINT '234') AS col_3 FROM with_0;
CREATE MATERIALIZED VIEW m3 AS SELECT (ARRAY['5F2pGCvMkw', 'vkjqkuJ0i3', 'hwkASzbxXL']) AS col_0, t_0.c16 AS col_1, (ARRAY['ZWVMvwity5', 'BD8q4k1ITz']) AS col_2 FROM alltypes2 AS t_0 WHERE t_0.c1 GROUP BY t_0.c13, t_0.c16;
CREATE MATERIALIZED VIEW m4 AS SELECT avg((SMALLINT '799')) FILTER(WHERE true) AS col_0, t_1.l_extendedprice AS col_1 FROM m2 AS t_0 JOIN lineitem AS t_1 ON t_0.col_1 = t_1.l_receiptdate GROUP BY t_1.l_extendedprice;
CREATE MATERIALIZED VIEW m5 AS SELECT tumble_0.c10 AS col_0, tumble_0.c5 AS col_1, tumble_0.c10 AS col_2, (REAL '205') AS col_3 FROM tumble(alltypes1, alltypes1.c11, INTERVAL '94') AS tumble_0 WHERE tumble_0.c1 GROUP BY tumble_0.c1, tumble_0.c5, tumble_0.c3, tumble_0.c10, tumble_0.c11, tumble_0.c7, tumble_0.c13, tumble_0.c15;
CREATE MATERIALIZED VIEW m6 AS SELECT (TIMESTAMP '2022-09-06 05:35:58') AS col_0, hop_0.date_time AS col_1 FROM hop(bid, bid.date_time, INTERVAL '86400', INTERVAL '6739200') AS hop_0 GROUP BY hop_0.date_time HAVING true;
CREATE MATERIALIZED VIEW m7 AS WITH with_0 AS (SELECT t_1.col_1 AS col_0, t_1.col_1 AS col_1, DATE '2022-09-06' AS col_2, t_1.col_1 AS col_3 FROM m4 AS t_1 GROUP BY t_1.col_1) SELECT (398) AS col_0, TIME '06:35:57' AS col_1 FROM with_0 WHERE false;
CREATE MATERIALIZED VIEW m8 AS WITH with_0 AS (SELECT (INT '798') AS col_0 FROM m6 AS t_1 FULL JOIN alltypes2 AS t_2 ON t_1.col_1 = t_2.c11 AND t_2.c1 GROUP BY t_2.c8, t_1.col_1, t_2.c3, t_2.c10, t_2.c7, t_2.c2, t_2.c13, t_1.col_0) SELECT (INTERVAL '86400') AS col_0 FROM with_0 WHERE true;
CREATE MATERIALIZED VIEW m9 AS SELECT t_1.c3 AS col_0, (TIME '06:35:59' + (INTERVAL '29018')) AS col_1, (t_1.c3 * (SMALLINT '0')) AS col_2, t_1.c3 AS col_3 FROM m0 AS t_0 JOIN alltypes1 AS t_1 ON t_0.col_0 = t_1.c9 GROUP BY t_1.c3, t_1.c14 HAVING false;
