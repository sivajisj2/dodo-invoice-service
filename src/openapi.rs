use axum::{Json, response::Html};
use serde_json::Value;

pub async fn spec() -> Json<Value> {
    let document: Value = serde_json::from_str(include_str!("../openapi.json"))
        .expect("openapi.json must contain valid JSON");

    Json(document)
}

pub async fn docs() -> Html<&'static str> {
    Html(
        r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Dodo Invoice Service API</title>
  <link rel="stylesheet"
        href="https://unpkg.com/swagger-ui-dist@5/swagger-ui.css">
</head>
<body>
  <div id="swagger-ui"></div>
  <script src="https://unpkg.com/swagger-ui-dist@5/swagger-ui-bundle.js"></script>
  <script>
    window.onload = () => {
      SwaggerUIBundle({
        url: "/openapi.json",
        dom_id: "#swagger-ui",
        deepLinking: true,
        persistAuthorization: false
      });
    };
  </script>
</body>
</html>"##,
    )
}
