-- MariaDB11.4 permits the same CHECK name on different tables. MySQL8.4 does not.
CREATE TABLE meta_check_other(z INT,CONSTRAINT pair_nonnegative CHECK(z<100));
