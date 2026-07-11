CREATE OR REPLACE PROCEDURE USP_NEW_ORDER_V16 (
    p_order_id IN NUMBER,
    p_qty IN NUMBER,
    p_price IN NUMBER,
    p_multiplier IN NUMBER DEFAULT 1
) AS
    v_total NUMBER;
BEGIN
    -- compute total
    v_total := p_qty * p_price * p_multiplier;
    INSERT INTO TORDER (order_id, qty, price, multiplier) VALUES (p_order_id, p_qty, p_price, p_multiplier);
    SPI_CHECKBUYLIMIT(p_order_id);
END;
/

CREATE OR REPLACE PROCEDURE SPI_CHECKBUYLIMIT (
    p_order_id IN NUMBER
) AS
    v_limit NUMBER;
BEGIN
    SELECT buy_limit INTO v_limit FROM TCLIENT LIMIT WHERE rownum = 1;
    UPDATE TORDER SET status = 'CHECKED' WHERE order_id = p_order_id;
END;
/

CREATE OR REPLACE FUNCTION F_GET_TOTAL (p_order_id IN NUMBER) RETURN NUMBER AS
    v NUMBER;
BEGIN
    SELECT qty * price INTO v FROM TORDER WHERE order_id = p_order_id;
    RETURN v;
END;
