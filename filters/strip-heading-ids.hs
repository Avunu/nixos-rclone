{-# LANGUAGE OverloadedStrings #-}
import Text.Pandoc.JSON

main :: IO ()
main = toJSONFilter stripHeadingIds

stripHeadingIds :: Block -> Block
stripHeadingIds (Header level (_, classes, kvs) inlines) =
  Header level ("", classes, kvs) inlines
stripHeadingIds blk = blk
