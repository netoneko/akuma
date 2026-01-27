// Simple CGI test script
// Arguments: script_path, method, query_string

// Read stdin (for POST data)
var postData = readStdin();

// Output CGI headers
console.log("Content-Type: text/html");
console.log("");  // Blank line separates headers from body

// Output body
console.log("<html>");
console.log("<head><title>CGI Test</title></head>");
console.log("<body>");
console.log("<h1>Meow from CGI!</h1>");
console.log("<p>This is a JavaScript CGI script running on Akuma.</p>");
if (postData && postData.length > 0) {
    console.log("<h2>POST Data Received:</h2>");
    console.log("<pre>" + postData + "</pre>");
}
console.log("</body>");
console.log("</html>");
