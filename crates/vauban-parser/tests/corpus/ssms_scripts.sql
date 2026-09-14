-- Batches in the style of what SQL Server Management Studio generates when scripting
-- objects: SET ANSI_NULLS ON / SET QUOTED_IDENTIFIER ON at the head, bracketed type
-- names, CONSTRAINT … PRIMARY KEY CLUSTERED (… ASC) WITH (…) ON [PRIMARY], ALTER TABLE …
-- WITH CHECK CHECK CONSTRAINT, tabs and doubled spaces where the tool puts them.
-- Written from general knowledge of the shapes this tool produces; no
-- table, column or schema of an identifiable project. The `GO` lines SSMS writes between
-- statements are client-side and never reach the server: they are not written here.
--
-- `SET QUOTED_IDENTIFIER ON` at the head of a batch has NO effect on the lexing of that
-- batch: `ParseOptions` is fixed when `parse_batch` is called, and the parser never
-- re-reads its own statements. No batch below therefore depends on that switch: there is
-- no `"…"` whose meaning would change with it.
--
-- Format: a line `-- @batch <name>` opens a batch, everything up to the next one belongs
-- to it, comments included. This format is local to this corpus.

-- @batch ssms_script_table_as_create_full
/****** Object:  Table [dbo].[Orders]    Script Date: 01/01/2024 12:00:00 ******/
SET ANSI_NULLS ON
SET QUOTED_IDENTIFIER ON
CREATE TABLE [dbo].[Orders](
	[Id] [int] IDENTITY(1,1) NOT NULL,
	[CustomerId] [int] NOT NULL,
	[OrderDate] [datetime2](7) NOT NULL,
	[Total] [decimal](18, 2) NOT NULL,
	[Status] [nvarchar](20) NOT NULL,
	[Notes] [nvarchar](max) NULL,
 CONSTRAINT [PK_Orders] PRIMARY KEY CLUSTERED 
(
	[Id] ASC
)WITH (PAD_INDEX = OFF, STATISTICS_NORECOMPUTE = OFF, IGNORE_DUP_KEY = OFF, ALLOW_ROW_LOCKS = ON, ALLOW_PAGE_LOCKS = ON, OPTIMIZE_FOR_SEQUENTIAL_KEY = OFF) ON [PRIMARY]
) ON [PRIMARY] TEXTIMAGE_ON [PRIMARY]

-- @batch ssms_script_table_without_lob_column
SET ANSI_NULLS ON
SET QUOTED_IDENTIFIER ON
CREATE TABLE [dbo].[Customers](
	[Id] [int] IDENTITY(1,1) NOT NULL,
	[Name] [nvarchar](200) NOT NULL,
	[Email] [nvarchar](256) NULL,
	[CreatedAt] [datetime2](7) NOT NULL,
	[IsActive] [bit] NOT NULL,
	[RowVersion] [timestamp] NOT NULL,
 CONSTRAINT [PK_Customers] PRIMARY KEY CLUSTERED 
(
	[Id] ASC
)WITH (PAD_INDEX = OFF, STATISTICS_NORECOMPUTE = OFF, IGNORE_DUP_KEY = OFF, ALLOW_ROW_LOCKS = ON, ALLOW_PAGE_LOCKS = ON, OPTIMIZE_FOR_SEQUENTIAL_KEY = OFF) ON [PRIMARY],
 CONSTRAINT [UQ_Customers_Email] UNIQUE NONCLUSTERED 
(
	[Email] ASC
)WITH (PAD_INDEX = OFF, STATISTICS_NORECOMPUTE = OFF, IGNORE_DUP_KEY = OFF, ALLOW_ROW_LOCKS = ON, ALLOW_PAGE_LOCKS = ON, OPTIMIZE_FOR_SEQUENTIAL_KEY = OFF) ON [PRIMARY]
) ON [PRIMARY]

-- @batch ssms_script_table_plain_constraints
SET ANSI_NULLS ON
SET QUOTED_IDENTIFIER ON
CREATE TABLE [dbo].[OrderLines](
	[Id] [bigint] IDENTITY(1,1) NOT NULL,
	[OrderId] [int] NOT NULL,
	[ProductId] [int] NOT NULL,
	[Quantity] [int] NOT NULL,
	[UnitPrice] [money] NOT NULL,
	[LineTotal]  AS ([Quantity]*[UnitPrice]) PERSISTED,
 CONSTRAINT [PK_OrderLines] PRIMARY KEY CLUSTERED 
(
	[Id] ASC
) ON [PRIMARY]
) ON [PRIMARY]

-- @batch ssms_add_default_constraint_for_column
ALTER TABLE [dbo].[Orders] ADD  CONSTRAINT [DF_Orders_Status]  DEFAULT (N'New') FOR [Status]

-- @batch ssms_with_check_add_foreign_key
ALTER TABLE [dbo].[Orders]  WITH CHECK ADD  CONSTRAINT [FK_Orders_Customers] FOREIGN KEY([CustomerId])
REFERENCES [dbo].[Customers] ([Id])

-- @batch ssms_check_constraint_after_add
ALTER TABLE [dbo].[Orders] CHECK CONSTRAINT [FK_Orders_Customers]

-- @batch ssms_with_check_add_check_constraint
ALTER TABLE [dbo].[Orders]  WITH CHECK ADD  CONSTRAINT [CK_Orders_Total] CHECK  (([Total]>=(0)))

-- @batch ssms_with_check_check_constraint_all
ALTER TABLE [dbo].[Orders] WITH CHECK CHECK CONSTRAINT ALL
ALTER TABLE [dbo].[Orders] WITH NOCHECK CHECK CONSTRAINT [CK_Orders_Total]
ALTER TABLE [dbo].[Orders] NOCHECK CONSTRAINT [FK_Orders_Customers], [CK_Orders_Total]

-- @batch ssms_script_index_with_include_and_options
SET ANSI_PADDING ON
CREATE NONCLUSTERED INDEX [IX_Orders_CustomerId] ON [dbo].[Orders]
(
	[CustomerId] ASC
)
INCLUDE([OrderDate],[Total]) WITH (PAD_INDEX = OFF, STATISTICS_NORECOMPUTE = OFF, SORT_IN_TEMPDB = OFF, DROP_EXISTING = OFF, ONLINE = OFF, ALLOW_ROW_LOCKS = ON, ALLOW_PAGE_LOCKS = ON, OPTIMIZE_FOR_SEQUENTIAL_KEY = OFF) ON [PRIMARY]

-- @batch ssms_script_unique_index_filtered
SET ANSI_PADDING ON
CREATE UNIQUE NONCLUSTERED INDEX [IX_Customers_Email] ON [dbo].[Customers]
(
	[Email] ASC
)
WHERE ([Email] IS NOT NULL)
WITH (PAD_INDEX = OFF, STATISTICS_NORECOMPUTE = OFF, SORT_IN_TEMPDB = OFF, IGNORE_DUP_KEY = OFF, DROP_EXISTING = OFF, ONLINE = OFF, ALLOW_ROW_LOCKS = ON, ALLOW_PAGE_LOCKS = ON) ON [PRIMARY]

-- @batch ssms_script_database_as_create
USE [master]
CREATE DATABASE [AppDb]
 CONTAINMENT = NONE
 ON  PRIMARY 
( NAME = N'AppDb', FILENAME = N'/var/opt/mssql/data/AppDb.mdf' , SIZE = 8192KB , MAXSIZE = UNLIMITED, FILEGROWTH = 65536KB )
 LOG ON 
( NAME = N'AppDb_log', FILENAME = N'/var/opt/mssql/data/AppDb_log.ldf' , SIZE = 8192KB , MAXSIZE = 2048GB , FILEGROWTH = 65536KB )
 WITH CATALOG_COLLATION = DATABASE_DEFAULT, LEDGER = OFF
ALTER DATABASE [AppDb] SET COMPATIBILITY_LEVEL = 160
ALTER DATABASE [AppDb] SET ANSI_NULL_DEFAULT OFF 
ALTER DATABASE [AppDb] SET RECOVERY FULL 
ALTER DATABASE [AppDb] SET  MULTI_USER 
ALTER DATABASE [AppDb] SET READ_WRITE 

-- @batch ssms_use_master_create_database_with_collation
USE [master]
CREATE DATABASE [AppDb_Test] COLLATE Latin1_General_100_CI_AS_SC
ALTER DATABASE [AppDb_Test] SET RECOVERY SIMPLE
ALTER DATABASE [AppDb_Test] SET READ_COMMITTED_SNAPSHOT ON WITH ROLLBACK IMMEDIATE

-- @batch ssms_select_top_1000_rows
/****** Script for SelectTopNRows command from SSMS  ******/
SELECT TOP (1000) [Id]
      ,[CustomerId]
      ,[OrderDate]
      ,[Total]
      ,[Status]
      ,[Notes]
  FROM [AppDb].[dbo].[Orders]

-- @batch ssms_drop_table_script
/****** Object:  Table [dbo].[Orders]    Script Date: 01/01/2024 12:00:00 ******/
DROP TABLE [dbo].[Orders]

-- @batch ssms_generate_scripts_data_only
SET IDENTITY_INSERT [dbo].[Customers] ON 
INSERT [dbo].[Customers] ([Id], [Name], [Email], [CreatedAt], [IsActive]) VALUES (1, N'Alpha', N'alpha@example.test', CAST(N'2024-01-01T00:00:00.0000000' AS DateTime2), 1)
INSERT [dbo].[Customers] ([Id], [Name], [Email], [CreatedAt], [IsActive]) VALUES (2, N'Beta', NULL, CAST(N'2024-02-01T00:00:00.0000000' AS DateTime2), 0)
SET IDENTITY_INSERT [dbo].[Customers] OFF

-- @batch ssms_edit_top_200_rows_update
UPDATE TOP (200) [dbo].[Products]
SET [Price] = [Price] * 0.9
WHERE [Discontinued] = 1

-- @batch ssms_drop_index_script
/****** Object:  Index [IX_Orders_CustomerId]    Script Date: 01/01/2024 12:00:00 ******/
DROP INDEX [IX_Orders_CustomerId] ON [dbo].[Orders] WITH ( ONLINE = OFF )
