-- Batches of control flow, as sent by a maintenance script or a job step: several
-- DECLAREs, WHILE, IF/ELSE, BEGIN TRAN … COMMIT, PRINT, EXEC dbo.p @a = 1, @b OUTPUT,
-- table variables, temporary tables, dynamic SQL.
-- Written from general knowledge of such scripts; no table, column or
-- schema of an identifiable project.
--
-- Format: a line `-- @batch <name>` opens a batch, everything up to the next one belongs
-- to it, comments included. This format is local to this corpus.

-- @batch proc_delete_in_batches_while_loop
SET NOCOUNT ON;
DECLARE @BatchSize int = 1000, @Rows int = 1, @Total int = 0;
WHILE @Rows > 0
BEGIN
    DELETE TOP (@BatchSize) FROM dbo.AuditLog WHERE At < DATEADD(day, -90, GETUTCDATE());
    SET @Rows = @@ROWCOUNT;
    SET @Total += @Rows;
END;
PRINT N'Deleted ' + CAST(@Total AS nvarchar(20)) + N' rows';

-- @batch proc_transfer_with_if_else_and_transaction
BEGIN TRAN;
UPDATE dbo.Accounts SET Balance = Balance - @Amount WHERE Id = @From AND Balance >= @Amount;
IF @@ROWCOUNT = 0
BEGIN
    ROLLBACK TRAN;
    PRINT N'insufficient funds';
    RETURN;
END
ELSE
BEGIN
    UPDATE dbo.Accounts SET Balance = Balance + @Amount WHERE Id = @To;
    INSERT INTO dbo.Transfers (FromId, ToId, Amount, At) VALUES (@From, @To, @Amount, SYSDATETIME());
    COMMIT TRAN;
END

-- @batch proc_exec_with_return_status_and_output
DECLARE @NewId int, @Rc int;
EXEC @Rc = dbo.usp_CreateOrder @CustomerId = 42, @Total = 99.90, @NewId = @NewId OUTPUT;
IF @Rc <> 0 PRINT N'usp_CreateOrder failed';
SELECT @NewId AS NewId, @Rc AS ReturnCode;

-- @batch proc_exec_positional_and_default_args
EXEC dbo.usp_Rebuild 'dbo', 'Orders', 1;
EXECUTE dbo.usp_Notify @Channel = N'ops', @Message = N'rebuild done', @Priority = DEFAULT;
EXEC sp_updatestats;

-- @batch proc_dynamic_sql_and_sp_executesql
DECLARE @sql nvarchar(max) = N'SELECT COUNT(*) FROM ' + QUOTENAME(@Schema) + N'.' + QUOTENAME(@Table);
EXEC (@sql);
EXEC sp_executesql N'SELECT * FROM dbo.Orders WHERE Id = @id', N'@id int', @id = @OrderId;
EXEC ('SELECT 1 AS [Empty]');

-- @batch proc_named_transaction_and_savepoint
BEGIN TRANSACTION OuterTran;
SAVE TRANSACTION BeforeInsert;
INSERT INTO dbo.Log (Message) VALUES (N'start');
IF @@ERROR <> 0 ROLLBACK TRANSACTION BeforeInsert;
COMMIT TRANSACTION OuterTran;

-- @batch proc_loop_with_break_and_continue
DECLARE @i int = 0;
DECLARE @max int = (SELECT MAX(Id) FROM dbo.Queue);
WHILE @i < @max
BEGIN
    SET @i = @i + 1;
    IF NOT EXISTS (SELECT 1 FROM dbo.Queue WHERE Id = @i) CONTINUE;
    IF @i > 1000 BREAK;
    UPDATE dbo.Queue SET Processed = 1, ProcessedAt = GETDATE() WHERE Id = @i;
END

-- @batch proc_session_options_and_select_assign
SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED;
SET XACT_ABORT ON;
SET DATEFIRST 1;
SET LOCK_TIMEOUT 5000;
DECLARE @name nvarchar(100), @count int, @last datetime2;
SELECT @name = Name FROM dbo.Customers WHERE Id = @Id;
SELECT @count = COUNT(*), @last = MAX(OrderDate) FROM dbo.Orders WHERE CustomerId = @Id;
SELECT @name AS Name, @count AS Total, @last AS LastOrder;

-- @batch proc_table_variable_join
DECLARE @Ids TABLE (Id int NOT NULL PRIMARY KEY, Note nvarchar(50) NULL);
INSERT INTO @Ids (Id) VALUES (1), (2), (3);
SELECT o.* FROM dbo.Orders o INNER JOIN @Ids i ON i.Id = o.Id;
DELETE FROM @Ids WHERE Id = 2;

-- @batch proc_temp_table_workflow
IF OBJECT_ID('tempdb..#Work') IS NOT NULL DROP TABLE #Work;
CREATE TABLE #Work (Id int NOT NULL, Payload nvarchar(max) NULL, Done bit NOT NULL DEFAULT 0);
INSERT INTO #Work (Id, Payload) SELECT Id, Payload FROM dbo.Inbox WHERE Processed = 0;
UPDATE i SET i.Processed = 1, i.ProcessedAt = GETUTCDATE() FROM dbo.Inbox i INNER JOIN #Work w ON w.Id = i.Id;
SELECT COUNT(*) AS Moved FROM #Work;
DROP TABLE #Work;

-- @batch proc_truncate_and_reload
TRUNCATE TABLE dbo.StagingOrders;
INSERT INTO dbo.StagingOrders (Id, Total)
SELECT Id, Total FROM dbo.Orders WHERE OrderDate >= @Since;
INSERT INTO dbo.StagingLog DEFAULT VALUES;
PRINT 'reloaded';

-- @batch proc_nested_if_else_chain
IF @Mode = 1
    IF @Verbose = 1 PRINT N'mode 1, verbose' ELSE PRINT N'mode 1'
ELSE IF @Mode = 2
    PRINT N'mode 2'
ELSE
BEGIN
    PRINT N'other';
    SET @Mode = -1;
END

-- @batch proc_compound_assignments_and_bit_ops
DECLARE @flags int = 0, @n int = 10, @s nvarchar(50) = N'';
SET @flags |= 4;
SET @flags &= ~1;
SET @n *= 2;
SET @n -= 1;
SET @n %= 7;
SET @s += N'x';
SELECT @flags AS Flags, @n AS N, @s AS S, @flags ^ 255 AS Inverted, -@n AS Negated, - -@n AS Twice;
